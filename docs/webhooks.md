# Webhooks

Wardex POSTs notable events to every configured webhook (see
`[[webhooks]]` in `deploy/wardex.example.toml`). Webhooks are meant to wake a
client up; the client then pulls details from Wardex (CLI or MCP, see
[`mcp.md`](mcp.md)).

## Events

Default set: `question`, `approval`, `reminder`, `done`, `error`, `stuck`.
Any event type can be listed in `events`; message deltas are never sent.

| Event | When |
|---|---|
| `question` | The agent asked something and waits (`wardex answer`). |
| `approval` | The agent asks for permission (rare with full-access modes). |
| `reminder` | A question has waited `timeouts.question_reminder_secs`. |
| `stuck` | A question has waited `question_stuck_secs` (`reason: waiting_input`), or a running turn produced nothing for `idle_stuck_secs` while no tool ran (`reason: no_output`; `idle_stuck_secs = 0` turns this off). Idle sessions (no turn running) are never reported stuck. |
| `done` | A turn finished. Carries the final text, the CLI's turn note and the git state (branch, changed files). |
| `error` | A turn failed or the agent process died. |
| `ended` | The agent process ended (not in the default set). `payload.state` is the session's new state, `payload.reason` why it ended (see [`mcp.md`](mcp.md#session-records-and-summaries)); events recorded by older versions have no `reason`. |

## Request

```http
POST <url>
Content-Type: application/json
User-Agent: wardex/<version>
X-Wardex-Event: done
X-Wardex-Delivery: <session_id>-<seq>
X-Wardex-Signature: sha256=<hex HMAC-SHA256 of the body>   (if a secret is set)
```

```json
{
  "event": {
    "session_id": "3f9c01ab",
    "seq": 42,
    "ts_ms": 1792345000000,
    "agent": "bot-a",
    "task": "fix-login",
    "driver": "claude",
    "type": "done",
    "payload": {
      "result": "Fixed the redirect and added a test.",
      "interrupted": false,
      "note": "login redirect fixed",
      "git": {"branch": "main", "changed_files": ["src/login.rs"]}
    }
  },
  "session": {
    "id": "3f9c01ab",
    "state": "idle",
    "driver": "claude",
    "agent": "bot-a",
    "task": "fix-login",
    "cwd": "/srv/repos/app",
    "driver_session_id": "…",
    "pending_questions": 0
  }
}
```

`X-Wardex-Delivery` is stable across retries; use it to drop duplicates.

## Routing

Every `[[webhooks]]` table is one target. Several can point at different bots:

```toml
[[webhooks]]
name = "bot-a"                  # used in logs instead of the URL
url_env = "WARDEX_HOOK_BOT_A_URL"
hmac_secret_env = "WARDEX_HOOK_BOT_A_SECRET"
agents = ["bot-a"]              # only sessions started with --agent bot-a
events = ["question", "approval", "reminder", "done", "error", "stuck"]

[[webhooks]]
name = "oncall"
url_env = "WARDEX_HOOK_ONCALL_URL"
agents = ["oncall"]             # its own sessions…
events = ["question", "approval", "reminder", "done", "error", "stuck"]
escalate = ["error", "stuck"]   # …plus errors and stuck sessions of everyone
```

A target gets an event when:

- the event type is in `escalate` (any session, labeled or not), or
- the type is in `events` **and** the session matches `agents` and `tasks`.

`agents` and `tasks` are lists of globs (`*` any run of characters, `?` one
character), e.g. `tasks = ["fix-*"]`. An empty or missing list matches every
session. A non-empty list never matches a session without that label, so
events of sessions started without `--agent` only go to targets without an
`agents` filter (and to `escalate`). Each target gets an event at most once,
even if it matches both ways.

## Changing webhooks without a restart

URLs and secrets live in the env file (`daemon.env_file`, default
`~/.config/wardex/env`, mode 600), as `KEY=VALUE` lines. The daemon reads it
itself at start and again on reload; its values win over the process env.

```sh
wardex daemon reload             # or: systemctl --user reload wardex (SIGHUP)
```

Reload re-reads the config and env files and switches webhooks over between
two events: unchanged targets keep their queue, removed ones finish what is
already queued, new ones start with the next event. Running sessions are not
touched. Other changed sections (timeouts, drivers, …) are reported as
`restart_needed`; apply them with `wardex daemon restart --drain <secs>`.

## Delivery

- One queue per webhook; events are delivered in order.
- Retries on network errors, timeouts, 5xx, 408 and 429, with exponential
  backoff starting at 1 s (capped at 60 s), up to `max_attempts`.
- Other 4xx responses are not retried.
- URLs, header values and secrets are read from env vars and never logged;
  logs refer to a webhook by its `name` (default `webhooks[<index>]`).
- `tool_call` events in webhooks carry the summaries only; the full tool
  input/output is available over `tool_output`.

## Verifying the signature

```python
import hashlib, hmac

def valid(secret: bytes, body: bytes, header: str) -> bool:
    expected = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, header)
```
