# MCP

Every CLI capability is an MCP tool, and the tools return the same JSON as the
CLI's `--json` output. Bots use them to start sessions, answer questions and
read results; webhooks ([`webhooks.md`](webhooks.md)) tell them when to look.

During a persistence outage, successful actions can return `applied: true`,
`persisted: false` and `warnings`: the agent command was accepted, so do not
resend it. The writer retains accepted writes and retries automatically.
Live `list`/`status` and `stop`/`interrupt` remain available. Degraded array
results use an object with `data`, `persisted` and `warnings`; ordinary healthy
results retain their existing shape. An admission error means no agent command
was applied (for example, when the bounded write queue is full).

Two transports serve the same tools:

- **stdio:** `wardex mcp --stdio` forwards every call to the running daemon
  over its Unix socket. No token or HTTP needed; a client on another machine
  reaches it over SSH, e.g. with the command `ssh <host> wardex mcp --stdio`.
- **Streamable HTTP:** `http://<http_listen>/mcp`, served by the daemon when a
  token is set, for clients that can only connect by URL (usually through a
  tunnel).

## stdio over SSH

Configure the MCP client to run a command instead of a URL:

```json
{
  "mcpServers": {
    "wardex": {
      "command": "ssh",
      "args": ["<host>", "~/.local/bin/wardex", "mcp", "--stdio"]
    }
  }
}
```

Non-interactive SSH sessions often lack `~/.local/bin` in `PATH`, hence the
full path. The SSH login is the access control: whoever can run commands as
that user can drive Wardex anyway. If the daemon is not running, tools return
an `unavailable:` error.

## Enabling HTTP

HTTP (and with it MCP) is only served when the token env var
(`auth.token_env`, default `WARDEX_TOKEN`) is set. Without a token the daemon
logs a warning and serves only the Unix socket.

```sh
openssl rand -hex 32    # → WARDEX_TOKEN in ~/.config/wardex/env
```

Every `/mcp` request needs `Authorization: Bearer <token>`. Wrong or missing
tokens get `401`; once `http.auth_failures_per_minute` (default 30) such
requests arrive in a minute, unauthenticated requests get `429` until the
minute is over. Requests with the right token are never limited.

`/mcp` also checks the `Host` header against `localhost`, `127.0.0.1`, `::1`
and `http.allowed_hosts` (DNS rebinding protection). Behind a tunnel, add the
public hostname there (or `WARDEX_ALLOWED_HOSTS`); see
[`deploy/tunnel.md`](../deploy/tunnel.md). Requests with another host get `403`.

`/healthz` needs no token and returns `{"ok": true, "version": …}`; with the
token it also reports `live_sessions`.

## Transport

- Stateless: no `Mcp-Session-Id`, every request stands alone, so clients keep
  working across daemon restarts.
- Responses are plain `application/json` (no SSE stream to hold open).
- Protocol versions from `2025-03-26` on are accepted.

Any client that speaks Streamable HTTP and can send a custom header works.
Example for Claude Code (`--mcp-config <file>`):

```json
{
  "mcpServers": {
    "wardex": {
      "type": "http",
      "url": "https://<your-hostname>/mcp",
      "headers": { "Authorization": "Bearer ${WARDEX_TOKEN}" }
    }
  }
}
```

## Tools

Session ids can be shortened to any unique prefix.

| Tool | Arguments | Returns |
|---|---|---|
| `run` | `driver` (`claude`/`codex`/`grok`/`agy`; `grok` and `agy` refuse `readonly`), `prompt`, `cwd` (absolute, on the Wardex host); optional `model`, `reasoning`, `worktree`, `branch`, `remove_worktree`, `readonly`, `agent`, `task`, `slice` | `{session, warnings}` |
| `overview` | optional `events` (latest events per session, default 3) | live sessions with state, pending questions, running tools, RSS/CPU, duration, `idle_since_ms`, `shutdown_in_ms` and latest events; counts per state; sessions ended in the last 24h |
| `list` | optional `all`; filters `agent`, `task` (globs with `*`, `?`), `ended` (ended sessions only), `resumable` (ended sessions `send` can continue), `origin` (`wardex`, `adopted`, `forked`) | session records (with `ended_reason`, `origin`), newest first, at most 500 |
| `status` | `id` | record, pending questions, running tools, RSS/CPU |
| `tail` | `id`; optional `since_seq`, `last`, `wait_secs` (max 60), `full` | events, oldest first |
| `tool_output` | `id`, `seq` (of either `tool_call` event of the call) | the call's full `input` and `output` |
| `summary` | `id` | final text, git state, durations, turn/event/tool counts |
| `pending_questions` | – | open questions and approvals of all sessions |
| `answer` | `id`; `text` **or** `answers` (item id → answer); optional `question_id` | `{session_id, question_id}` |
| `send` | `message`; `id`, **or** `latest` with `agent` and/or `task` globs (the newest matching session, live or ended) | `{session_id, resumed}` |
| `interrupt` | `id` | `{session_id}` |
| `stop` | `id` | `{session_id}` |
| `resume` | `id`; optional `message` | session record |
| `external_list` | optional `driver`, `cwd` (absolute), `under`, `since_secs`, `limit` (default 50), `include_managed` | `{sessions}`: sessions started outside Wardex, newest first, with `liveness` |
| `history` | `id` (Wardex id or external id); optional `driver`, `turns` (default 20), `before`, `tools` (`none`/`summary`/`full`), `max_bytes` (default 65536) | `{session, items, next_before, truncated, warnings}` |
| `adopt` | `id` (external id); optional `driver`, `agent`, `task`, `model`, `reasoning`, `readonly`, `message`, `fork` | `{session, adopted, resumed, source, warnings}` |

`external_list`, `history` and `adopt` are listed only when the daemon has both
`external.enabled` and `external.mcp` set (both off by default); see
[External sessions](#external-sessions).

Errors come back as tool results with `isError: true` and a text of the form
`<code>: <message>`, where the code is `not_found`, `ambiguous`, `invalid`,
`conflict` or `internal`.

### Questions

A question event (`tail`, `pending_questions`, `overview`) carries one or more
items:

```json
{"type": "question", "payload": {"question_id": "q1", "items": [
  {"id": "Which color?", "header": "Color", "text": "Which color?",
   "options": [{"label": "blue", "description": "…"}], "multi_select": false,
   "allow_other": true}
]}}
```

- One item: `answer {"id": "…", "text": "blue"}` (an option label or, when
  `allow_other` is set, free text).
- Several items: `answer {"id": "…", "answers": {"<item id>": "…", …}}`.
- Approvals (`type: approval`, id in `request_id`): `text` is `allow` or
  `deny`. `pending_questions` lists both kinds under `question_id`.

### Session records and summaries

- `model` is the model the CLI reports it resolved (e.g. a full model id when
  an alias or nothing was given).
- `reasoning` is the effort Wardex passed, or else the one the CLI reports.
  Codex reports its effective effort on `thread/start`; Claude Code does not
  report it, so for Claude it stays empty unless given per run or through
  `drivers.claude.default_reasoning`. Grok reports both model and effort; the
  `started` event carries what it reports, and a requested value Grok does not
  apply fails the session. Antigravity (`agy`) echoes only the model it was
  given and never reports the effort, so its `reasoning` is the requested
  value.
- `summary.duration_ms` runs from creation to the end of the last turn (to
  now while a turn runs). `last_turn_ended_ms` is when that turn ended and
  `last_turn_ms` how long it took, from its prompt.
- `ended_ms` is when the agent **process** ended. It stays empty while a
  session is `idle`: the process is alive and waits for the next message. It
  is set once the session stops, errors, or closes after
  `sessions.idle_shutdown_secs`.
- `ended_reason` says why the process ended (also `reason` on the `ended`
  event): `done` (the agent exited by itself), `stopped`, `idle_shutdown`
  (idle for `sessions.idle_shutdown_secs`), `idle_evicted` (more than
  `sessions.max_idle_live` idle sessions), `memory_pressure` (available memory
  below `sessions.min_mem_available_mb`), `error`, `drain`, `shutdown` (daemon
  stop or restart), `daemon_lost` (the daemon died while it ran) or `adopted`
  (created by `adopt`; no Wardex process has run yet). It is
  empty while the session is live and for sessions that ended before it was
  recorded. After `idle_shutdown`, `idle_evicted` and `memory_pressure` the
  work is unfinished: `send` continues it, and a `remove_worktree` worktree is
  kept until the session is stopped or ends by itself.
- `send` and `resume` fail with `conflict` if the session's working directory
  no longer exists (for example a removed worktree); nothing is started.

### Tool output

`tool_call` events carry a one-line `input_summary` and a 300-character
`result_summary`. Wardex also stores each call's full input and output (up to
256 KiB each). They are left out of `tail` (pass `full: true` to include
them) and of webhooks; `tool_output {"id": …, "seq": …}` returns them for one
call. CLI: `wardex tail <id> --full`, `wardex tool-output <id> <seq>`.

### External sessions

With `[external] enabled = true` and `mcp = true`, clients can see Claude Code
and Codex sessions that were started outside Wardex, for example in a
terminal, and read their conversation. Every MCP client then sees the user's
terminal history (Wardex has no roles), so only turn `mcp` on when that is
acceptable. Details: [`external-sessions.md`](external-sessions.md).

- `external_list {"cwd": "/srv/app", "under": true}`: sessions that started in
  that directory or below. `liveness.state` is `live` (open in a CLI process
  right now), `recent` (written within `external.quiet_secs`), `inactive` or
  `unknown`.
- `history {"id": "claude:1a2b3c4d"}`: the last 20 turns as `prompt`,
  `message`, `tool_call` and `compaction` items, read from the CLI's
  transcript every time (never copied into Wardex). Page back with
  `before` = `next_before`. Secrets are masked on a best-effort basis; treat
  the result as sensitive anyway. `history` also takes a Wardex session id.
- `adopt {"id": "claude:1a2b3c4d", "message": "…"}`: a Wardex session
  (`origin: adopted`) bound to the same native id; `send` continues it like
  any other session. Refused while the session is `live` or `recent`/`unknown`
  (there is no `force` over MCP). Unless `readonly` is set, the session
  continues with full permissions, whatever mode it was started in.
- `adopt {"id": "…", "fork": true, "message": "…"}`: a new session
  (`origin: forked`) that branches off the external one; the original is only
  read, so this also works while it is open elsewhere.

### Paging and waiting

`tail` without `since_seq` returns the last `last` (default 20) events. With
`since_seq`, it returns everything after that sequence number; pass the last
`seq` you saw to continue. Add `wait_secs` to block until the next event if
there is none yet, which lets a client follow a session without webhooks.
Message deltas are never returned; full messages are.

## The bot loop

1. `run` with `agent` (who you are) and `task` (what for) labels. Keep the id.
2. Wait for a webhook (`question`, `done`, `error`, `stuck`, `reminder`). It
   carries the session id and the event; anything else, pull over MCP.
3. On `question`: read it (from the webhook, `pending_questions` or `tail`)
   and `answer`.
4. On `done`: read `summary` (result text, changed files, branch). Either
   `send` the next instruction or leave the session alone.
5. **Do not `stop` a session because its work is done.** An idle session
   closes itself after `sessions.idle_shutdown_secs` (default 43200 s = 12 h;
   `overview` shows `shutdown_in_ms`), or earlier when more than
   `sessions.max_idle_live` sessions are idle or memory runs low, and `send`
   resumes it with its context later. Use `stop` only to abort work.
6. Use `overview` to see everything at once, e.g. after a bot restart.
7. To pick up earlier work without its id, `list` with `resumable: true` and
   your `agent`/`task` labels, or `send` with `latest: true` and the same
   labels.

Without webhooks, poll `overview` or follow one session with `tail` +
`since_seq` + `wait_secs`.

### Prompting bots

A short description the bot's instructions can include:

> Wardex (MCP server `wardex`) runs Claude Code, Codex, Grok Build and
> Antigravity sessions for you. Start one with `run` (`driver` claude, codex,
> grok or agy, absolute `cwd`, `agent` set
> to your name and `task` to a short label). Sessions work on their own. When
> one asks a question it shows up in `pending_questions`; reply with `answer`.
> When a turn is done, read `summary` and continue with `send` if needed.
> Do not `stop` finished sessions; idle ones close on their own and `send`
> resumes them. `overview` shows all sessions. Do not start a second session
> for work that is already running.

## Optional MCP Events

Wardex 0.2 can advertise native event definitions alongside these tools.
Enable deliberately with `[mcp_events] enabled = true`; follow
[`mcp-events.md`](mcp-events.md) for ownership, callback security, persistence,
replay, migration and platform acceptance. This does not replace generic
webhooks or enable monitoring without a platform subscription.

For event-triggered answers, pass `question_id` and the event's `request_seq`
as the optional `expected_question_seq` field of `answer`. This refuses an
old occurrence even when a resumed driver reuses its native request ID.
An interrupted/completed turn or native resolution cancels unanswered input.
Never interpret an old event as current approval, or auto-approve from payload
text. With Events enabled, `overview.mcp_events` exposes safe delivery health.
