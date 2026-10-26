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
| Grok Build | 1.0.70 | 2026-10-26 |

Fork (`--fork-session`, `thread/fork`) was checked against the same versions'
help text, argument validation and generated JSON schema (2026-10-19); the
external session readers were written against transcripts of Claude Code
2.1.295 – 2.1.302 and Codex 0.162.1 – 0.163.0 and are tested with synthetic
fixtures (`tests/fixtures/{claude,codex}/store/`, see
[`external-sessions.md`](external-sessions.md)).

The lifecycle fixes are tested with these recorded versions and temporary
shell processes; no live Claude/Codex CLI is called. The shared runner handles
`Drain` by closing stdin without a force-kill deadline for the leader, including
when stdout has already closed. After leader exit the manager reaps remaining
scope descendants with SIGTERM, escalating to SIGKILL and verifying exit before
drain/resume completes. Default tests use test-owned processes and an injected
scope controller; the real user-systemd drain test is explicitly ignored.
Explicit `Shutdown`/`Kill` retain their existing fallback.
Pending question/approval request ids belong to the live process, so drain
waits for answers and refuses restart on timeout instead of trying to replay
those ids into a resumed driver.

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
  so the resume id is known before the first event. The driver reports it in
  its spawn handle and the manager records it at once; `system/init` later
  confirms the same id.
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

### Fork

`--resume <source> --fork-session --session-id <new-uuid>`: a new session
that starts from the source's history; the source transcript is only read.
Claude 2.1.302 accepts `--session-id` together with `--resume` only with
`--fork-session` (its argument check says so), which lets Wardex record the
fork's id before the process starts, as for new sessions. `system/init` then
reports the new id. Verified against the 2.1.302 `--help` text and argument
validation; no fork transcript was recorded (the protocol after `init` is the
same as for a resume).

### Known limitations

- Because the session id is recorded when the process starts, a Claude
  session (or fork) whose process dies before its first event still has a
  resume id. Claude may not have written a transcript for it yet, so a later
  `resume`/`send` can fail with Claude's own "No conversation found" error
  instead of Wardex's "the agent never reported a session id". Start a new
  session (or adopt/fork the source again) in that case.

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

### Fork

`thread/fork {threadId, cwd, approvalPolicy, sandbox, model?,
excludeTurns:true}` instead of `thread/resume`. The result has the same shape
as `thread/resume`'s (`thread`, `model`, `reasoningEffort`, …) with the new
thread in `thread.id` and the source in `thread.forkedFromId`; turns then go
to the new thread. Only `threadId` is required (0.163.0 schema,
`ThreadForkParams`; optional `beforeTurnId`/`lastTurnId` are not used).
`fixtures/codex/fork.jsonl` is **derived** from the recorded `resume.jsonl`
and that schema by `tools/fixtures/derive_codex_fork.py`, not recorded from a
live fork.

## Grok Build

Recorded against Grok 1.0.70 (`grok agent stdio`) on Linux x86_64 with the
recording user's own login and settings. Statements marked *not recorded*
come from the ACP specification or Grok's bundled documentation and are only
covered by tests built from those schemas.

### Invocation

```text
grok agent --no-leader [--always-approve] [extra_args...] stdio
```

- The [Agent Client Protocol](https://agentclientprotocol.com) (ACP v1):
  JSON-RPC 2.0 over stdio, one agent process per Wardex session.
- **`--no-leader`**: Grok's config can route `grok agent` through a shared
  leader process; the session would then run outside its systemd scope.
- Authentication is Grok's own cached login. `session/new` works without an
  `authenticate` call when the user is logged in (`initialize` advertises
  `cached_token`). Wardex never calls `authenticate` and never reads Grok's
  credentials. Without a login the session fails with Grok's error.
- Grok reads the user's `~/.grok/config.toml`, plugins, hooks, skills and
  Claude-compatible settings (`.claude/settings.json`), like an interactive
  session. Sessions are written to `~/.grok/sessions/` and show up in
  `grok --resume`.

### Handshake and turns

```text
→ initialize {protocolVersion:1, clientCapabilities:{fs:{readTextFile:false,
              writeTextFile:false}, terminal:false}, clientInfo}
← result {agentCapabilities:{loadSession:true, sessionCapabilities:{resume, list,
          close}, ...}, authMethods}
→ session/new {cwd, mcpServers:[], _meta?:{yoloMode:false}}
← result {sessionId, models, configOptions:[{id:"model", currentValue},
          {id:"reasoning_effort", category:"thought_level", currentValue}]}
→ session/set_config_option {sessionId, configId, value}   (only to change one)
← result {configOptions}                                   (the full new list)
→ session/prompt {sessionId, prompt:[{type:"text", text}]}
← session/update notifications ...
← result {stopReason}
```

Grok also sends many `_x.ai/*` notifications (session setup phases, hooks,
models, queue, announcements, `sessions/changed`) and responses to ids it
made up itself (`skills-reload`). Wardex ignores them.

| `session/update` | Wardex mapping |
|---|---|
| `agent_message_chunk` | `message` delta; the text since the last tool call becomes a full `message`, the last one the turn's result |
| `tool_call` | `tool_call` started; name from `_meta["x.ai/tool"].name` (e.g. `run_terminal_command`), input from `rawInput` |
| `tool_call_update` with `status` `completed` / `failed` | `tool_call` completed / failed; output from `rawOutput.output_for_prompt` or the text content |
| `agent_thought_chunk`, `user_message_chunk`, `available_commands_update`, `session_info_update` | ignored |
| `config_option_update` | updates the observed model / effort |

`session/prompt` `stopReason`: `end_turn` → `done`; `cancelled` → `done`
(interrupted); anything else (`refusal`, `max_tokens`, `max_turn_requests`)
→ `error`.

### Model and effort

`--model` and `--reasoning-effort` on `grok agent` are **not** reliable: with
`--reasoning-effort low` the recorded session reported `xhigh`, the effort
stored in the user's settings for its default model. Wardex therefore sets
the requested values with `session/set_config_option` (model first, then
`reasoning_effort`) and reads the values back from the returned
`configOptions`. If Grok rejects a value or reports a different one, the
session fails before the first prompt instead of running with settings that
were not asked for. The `started` event carries the values Grok reports;
the session record keeps the requested ones. Effort values come from the
model (`low`, `medium`, `high`, `xhigh` on the recording host).

### Permissions

`drivers.grok.approval` selects the execution mode; Wardex never answers a
permission request on its own:

| Value | Passed to Grok |
|---|---|
| `inherit` (default) | nothing: the user's Grok settings decide |
| `ask` | `_meta: {yoloMode: false}` on `session/new` and `session/resume` |
| `always_approve` | `--always-approve` (deny rules and hooks still apply) |

Recorded behavior: with `yoloMode: false` Grok reports `yolo: false`, but
its own permission pipeline still decides first. On the recording host the
auto-review mode was seeded from a Claude-compatible `defaultMode`; Grok's
classifier allowed a harmless shell command and blocked a force push
(`tool_call_update` `failed`, "Auto mode blocked this action"), each after an
`_x.ai/session_notification` `pending_interaction` / `interaction_resolved`
pair. **No `session/request_permission` reached the client** in any
recording, and Wardex did not change the user's settings to force one.

*Not recorded:* an ACP `session/request_permission` becomes an `approval`
event (`kind` from `toolCall.kind`, summary from its title). An answer selects
the `allow_once` or `reject_once` option (`reject_always` if there is no
`reject_once`); Wardex never selects an `allow_always` option, and an allow
without an `allow_once` option is sent as `cancelled`.

### Questions (`ask_user_question`)

Grok's `ask_user_question` tool asks the client with an agent request:

```json
← {"jsonrpc":"2.0","id":0,"method":"_x.ai/ask_user_question","params":{
     "sessionId","toolCallId","mode":"default",
     "questions":[{"question":"Which color do you prefer?","multiSelect":null,
                   "options":[{"label":"red","description":"..."}, ...]}]}}
→ {"jsonrpc":"2.0","id":0,"result":{"outcome":"accepted",
     "answers":{"Which color do you prefer?":"blue"}}}
```

Answers are keyed by question **text** (verified: the tool completed with
`picked: ["blue"]` and the model answered `blue`). The result is an
internally tagged enum; its other outcomes are `cancelled`,
`chat_about_this` and `skip_interview` (from Grok's parse error). Wardex
sends `cancelled` when a question is declined; several selected options are
sent as an array (*not recorded*). A client that does not answer makes the
tool fail ("Failed to reach the client for user question"). Grok's own
timeout (`toolset.ask_user_question.timeout_secs`, default 1800 s) still
applies.

### Interrupt

`session/cancel` (a notification) ends the turn with `stopReason:
"cancelled"` within milliseconds; the running shell tool gets no completion
update. Per ACP, Wardex answers pending requests of the cancelled turn with
`cancelled` (*not recorded*: no request was pending in the recording).

### Resume

`session/resume {sessionId, cwd, mcpServers}` (advertised as
`sessionCapabilities.resume`) continues with the history intact and replays
nothing; the session keeps its model and effort. `session/load` would stream
the whole history again as `user_message_chunk` / `agent_message_chunk`, so
Wardex does not use it. Closing stdin ends the process with exit code 0.

### Not supported

- **Read-only.** `grok agent` offers no per-session read-only mode over ACP
  (no session modes are advertised). `--permission-mode plan` and
  `--sandbox` exist for interactive/headless `grok` but were not verified as
  write barriers. Wardex refuses `--readonly` for Grok.
- **Fork.** `--fork-session` is a headless flag; ACP has no fork call. Fork
  needs an external session reader, which Grok does not have in Wardex.
- **External sessions.** Grok's store is not read (`external list`,
  `history`, `adopt`).

### Downgrade

Wardex 0.2.x and earlier reject session rows with an unknown driver and a
`[drivers.grok]` config section. Once Grok sessions exist, rolling back means
restoring a database backup taken before the upgrade (an older daemon's
startup recovery fails on a Grok session that was live when it stopped).

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

### Input request resolution (0.2)

The recorded baseline remains Codex CLI 0.163.0; no additional live CLI
version is claimed. Synthetic protocol tests supplement that baseline:
`serverRequest/resolved` removes a request handle, and `turn/completed`
invalidates the remaining handles. Manager completion/interruption cancels
unanswered questions. `answer.expected_question_seq` can fence a native ID
reused in a later occurrence. The resolution message follows the current
[App Server guide](https://learn.chatgpt.com/docs/app-server); validate it
against generated schemas/recordings before changing supported CLI versions.
