# Drivers

Findings from the M1 spikes. The recorded transcripts behind every statement
here live in `crates/wardex-drivers/tests/fixtures/`. Re-record them with
`tools/fixtures/record_*.py` + `tools/fixtures/sanitize.py` whenever a tested
CLI version changes.

## Tested versions

| CLI | Version | Recorded |
|---|---|---|
| Claude Code | 2.1.302 | 2026-10-18 |
| Codex CLI | 0.163.0 | 2026-10-18 |

## Fixture format

One JSON object per line:

```json
{"t": 2.16, "dir": "out", "msg": { ... }}
```

- `t`: seconds since the process started (`null` if unknown).
- `dir`: `in` (written to the CLI's stdin), `out` (read from its stdout),
  `exit` (exit code).
- `msg`: the raw protocol message.

A mock process replays the `out` lines and checks that the driver writes the
expected `in` lines.

## Claude Code

### Invocation

```text
claude -p --input-format stream-json --output-format stream-json --verbose
       --permission-mode bypassPermissions
       --permission-prompt-tool stdio
       --session-id <uuid>            # new session
       --resume <uuid>                # instead of --session-id, to resume
       --model <model>
       [--include-partial-messages]
```

- **`--permission-prompt-tool stdio` is required for questions.** Without it,
  `AskUserQuestion` is not in the tool list at all in `-p` mode
  (`--permission-prompts host`, `--tools` and `--allowedTools` do not add it).
- **`--session-id <uuid>`** lets Wardex choose the Claude session id up front,
  so the resume id is known before the first event.
- The process stays alive between turns as long as stdin is open. Each new
  turn re-emits `system/init`. Closing stdin after a `result` ends it cleanly
  (exit 0).
- User settings, hooks and `CLAUDE.md` apply to Wardex sessions exactly as in
  an interactive session (hooks can block tool calls, `CLAUDE.md` changes the
  reply language, etc.).
- Like interactive sessions, Wardex sessions write Claude's own state:
  `~/.claude.json`, session transcripts under `~/.claude/projects/`, plan
  files under `~/.claude/plans/`, and auto memory if enabled. They show up in
  `claude --resume`.

### Output messages (stdout)

| Message | Meaning / Wardex mapping |
|---|---|
| `system/init` | `session_id`, `model`, `permissionMode`, `tools`, `claude_code_version`. Once per turn. First one → `started`. |
| `system/status` | `status: "requesting"` while waiting on the API. |
| `stream_event` | Only with `--include-partial-messages`. Raw API stream (`message_start`, `content_block_delta`, …). → `message` deltas, broadcast only. |
| `assistant` | One event **per content block** (`thinking`, `text`, `tool_use`); blocks of one API message share `message.id`. `text` → `message`; `tool_use` → `tool_call` (started). |
| `user` | Tool results (`tool_result`, optional `is_error`) → `tool_call` (completed/failed). Also `[Request interrupted by user]` after an interrupt. |
| `control_request` | `can_use_tool` (see below). |
| `control_response` | Acknowledges our `control_request`s. |
| `system/task_*`, `system/background_tasks_changed` | Background shell tasks. Informational. |
| `system/post_turn_summary` | `status_category`, `status_detail`, `needs_action`: a one-line model-written summary of the turn. Useful for `summary` and `overview`. |
| `system/hook_*`, `system/thinking_tokens`, `rate_limit_event` | Informational; not persisted. |
| `result` | End of a turn. `subtype` (`success`, `error_during_execution`, …), `is_error`, `result` (final text), `terminal_reason`, `num_turns`, `duration_ms`, `total_cost_usd`, `usage`. → `done` or `error`. |

### Questions (`AskUserQuestion`)

The CLI asks the host and **blocks until it answers** (verified with a 20 s
delay):

```json
{"type":"control_request","request_id":"<rid>","request":{
  "subtype":"can_use_tool","tool_name":"AskUserQuestion",
  "input":{"questions":[{"question":"...","header":"...","multiSelect":false,
           "options":[{"label":"Red","description":"..."}, ...]}]},
  "tool_use_id":"toolu_...","requires_user_interaction":true}}
```

Answer by allowing the tool with the answers added to its input, keyed by
question **text**:

```json
{"type":"control_response","response":{"subtype":"success","request_id":"<rid>",
  "response":{"behavior":"allow",
              "updatedInput":{"questions":[...unchanged...],
                              "answers":{"<question text>":"Blue"}}}}}
```

Declining (`{"behavior":"deny","message":"..."}`) turns into an `is_error`
tool result and the model carries on. Wardex uses this when a question is
cancelled.

One request can carry several questions.

### Permission requests

With `bypassPermissions`, ordinary tools (Bash, Write, …) never reach the
host; only interactive tools do (`AskUserQuestion`, `ExitPlanMode`). Any other
`can_use_tool` request is mapped to an `approval` event.

### Read-only (`--readonly`)

`--permission-mode plan` alone is **not** read-only for a host that allows
everything: in plan mode writes arrive as `can_use_tool` requests, and allowing
them writes the file. A read-only session therefore runs in plan mode and the
driver **denies every `can_use_tool` request except `AskUserQuestion`**
(including `ExitPlanMode`). Verified: the file is not created and the model
reports that it could not write.

Notes: plan mode may switch model according to user settings, and writes its
plan file under `~/.claude/plans/` regardless (seen again in the acceptance
run).

### Interrupt

```json
{"type":"control_request","request_id":"<id>","request":{"subtype":"interrupt"}}
```

Acknowledged with a `control_response`; running background tasks are killed;
the turn ends with `result` `subtype: "error_during_execution"`,
`is_error: true`, `terminal_reason: "aborted_streaming"`. **Wardex must map an
interrupt it requested to `done` (interrupted), not `error`.** Closing stdin
afterwards exits with code 1, which is not an error either.

### Model and effort

The `init` message reports the resolved model (`model`), but not the
reasoning effort; the effort is only known when Wardex passes `--effort`.

### Resume

Start a new process with `--resume <session_id>`; history is intact (verified
by asking about the previous turn). The session id stays the same.

## Codex

### Invocation

```text
codex app-server [--enable default_mode_request_user_input]
```

Newline-delimited JSON-RPC over stdio, without a `"jsonrpc"` field. One
app-server process per Wardex session (so each session has its own systemd
scope).

Types can be generated with
`codex app-server generate-json-schema --out <dir>`. The protocol has 100+
client methods; Wardex only uses the ones below.

### Handshake and turns

```text
→ initialize {clientInfo:{name,version}, capabilities:{experimentalApi:true}}
← result {userAgent, codexHome, platformOs, ...}
→ initialized                                  (notification)
→ thread/start {cwd, approvalPolicy:"never", sandbox:"danger-full-access", model?}
← result {thread:{id, ...}, model, reasoningEffort, ...}
                                               thread id = resume id; model and
                                               effort are the resolved values
→ turn/start {threadId, input:[{type:"text",text}], effort?, model?}
← result {turn:{id, status:"inProgress"}}
← notifications ... turn/completed {turn:{status, error, items}}
```

- `effort` (per turn) takes the values the model advertises in `model/list`
  (e.g. `low` … `max`, `ultra`). Wardex passes `--reasoning` through.
- The user's `~/.codex/config.toml` applies (default model, effort, MCP
  servers), so Wardex should always pass `model` and `effort` explicitly when
  given.
- Starting an app-server can trigger Codex's own background memory
  consolidation (`~/.codex/memories/`) over the user's earlier sessions. This
  is Codex behavior, independent of Wardex.
- **Codex writes to the user's config.** `thread/start` in a git repository
  that is not yet trusted appends `[projects."<cwd>"] trust_level = "trusted"`
  to `~/.codex/config.toml` (observed in 0.163.0; non-git directories were not
  added). This is Codex's own behavior, not Wardex's. Wardex itself never
  writes CLI config files.

### Notifications

| Method | Wardex mapping |
|---|---|
| `thread/started`, `turn/started` | `started` (first turn) |
| `item/started` / `item/completed` with `item.type` | `agentMessage` (`phase`: `commentary` / `final_answer`) → `message`; `commandExecution` (`command`, `exitCode`, `aggregatedOutput`), `fileChange`, `mcpToolCall`, `webSearch`, … → `tool_call` started / completed; `userMessage`, `reasoning` → ignored |
| `item/agentMessage/delta` and other `*Delta` | `message` deltas, broadcast only |
| `thread/status/changed` | `{type:"active", activeFlags:["waitingOnUserInput" \| "waitingOnApproval"]}` / `{type:"idle"}`: drives session state |
| `turn/completed` | `turn.status`: `completed` → `done`; `interrupted` → `done` (interrupted); `failed` → `error` |
| `serverRequest/resolved` | A server request was answered |
| `error`, `warning`, `deprecationNotice`, `configWarning` | logged; `error` → `error` |
| `thread/tokenUsage/updated`, `account/*`, `mcpServer/*`, `remoteControl/*`, … | informational |

### Questions (`item/tool/requestUserInput`)

Only available with the **under-development** feature
`default_mode_request_user_input` (`--enable default_mode_request_user_input`);
Codex emits a `warning` notification when it is on. It works as expected in
0.163.0:

```json
← {"id":0,"method":"item/tool/requestUserInput","params":{
     "threadId","turnId","itemId","isBlocking":true,
     "questions":[{"id":"color_preference","header":"Color","question":"...",
                   "isOther":true,"isSecret":false,
                   "options":[{"label":"Red","description":"..."}]}]}}
→ {"id":0,"result":{"answers":{"color_preference":{"answers":["Blue"]}}}}
```

Answers are keyed by question **id**. While waiting, the thread status has
`activeFlags: ["waitingOnUserInput"]`. Wardex enables this through
`drivers.codex.request_user_input` (see the scope).

### Approvals

With `approvalPolicy: "never"` none arrive. Format recorded with
`approvalPolicy: "untrusted"`:

```json
← {"id":0,"method":"item/commandExecution/requestApproval","params":{
     "kind":"command","threadId","turnId","itemId","command","cwd",
     "availableDecisions":["accept",{"acceptWithExecpolicyAmendment":{...}},"cancel"]}}
→ {"id":0,"result":{"decision":"decline"}}
```

Decisions: `accept`, `acceptForSession`, `acceptWithExecpolicyAmendment`,
`decline`, `cancel`. Related requests: `item/fileChange/requestApproval`,
`item/permissions/requestApproval`, `mcpServer/elicitation/request`. All map to
`approval`.

### Read-only

`sandbox: "read-only"` with `approvalPolicy: "never"`: shell writes fail and
the model reports it. No extra handling needed.

### Interrupt

```text
→ turn/interrupt {threadId, turnId}
← result {}
← turn/completed {turn:{status:"interrupted"}}
```

### Resume

New process, `initialize`, then
`thread/resume {threadId, cwd, approvalPolicy, sandbox, excludeTurns:true}`,
then `turn/start` as usual. History is intact. Without `excludeTurns: true`
Codex sends a `deprecationNotice` and the full history.

## systemd scopes

```text
systemd-run --user --scope --quiet --expand-environment=no
            --unit wardex-<session-id> [--slice <slice>] -- <cli> <args...>
```

- `systemd-run --scope` execs the command: **the spawned PID is the agent's
  PID**, and stdin/stdout pipes pass straight through.
- **`--expand-environment=no` is required.** Otherwise systemd-run expands
  `$VAR` / `$$` in the arguments.
- Unit names must be unique; use the Wardex session id. The scope disappears
  shortly after the process exits.
- Slice names with dashes are hierarchical: `wardex-build.slice` is created
  under `wardex.slice`.
- Resource usage: read `/proc/<pid>/cgroup` (`0::<path>`), then
  `/sys/fs/cgroup<path>/memory.current` (bytes, whole process tree) and
  `cpu.stat` (`usage_usec`).
- Without `loginctl enable-linger`, user scopes stop at logout.
- Wardex adds `--collect`, so a scope whose process failed is unloaded too,
  and kills the whole scope when a session ends.
- `cargo test -p wardex-core -- --ignored systemd` checks all of the above on
  a host with a systemd user manager.
