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
| `stuck` | A question has waited `question_stuck_secs` (`reason: waiting_input`), or a running turn produced nothing for `idle_stuck_secs` while no tool ran (`reason: no_output`). |
| `done` | A turn finished. Carries the final text, the CLI's turn note and the git state (branch, changed files). |
| `error` | A turn failed or the agent process died. |

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

## Delivery

- One worker per webhook; events are delivered in order.
- Retries on network errors, timeouts, 5xx, 408 and 429, with exponential
  backoff starting at 1 s (capped at 60 s), up to `max_attempts`.
- Other 4xx responses are not retried.
- URLs, header values and secrets are read from env vars and never logged;
  logs refer to a webhook by its position in the config.

## Verifying the signature

```python
import hashlib, hmac

def valid(secret: bytes, body: bytes, header: str) -> bool:
    expected = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, header)
```
