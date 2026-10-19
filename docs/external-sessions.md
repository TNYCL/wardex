# External sessions

Claude Code and Codex sessions started outside Wardex (in a terminal, an IDE
or a desktop app) can be listed, read and continued by Wardex:

- **Discovery** (`wardex external list`, MCP `external_list`): sessions found
  in the CLIs' own transcript stores, filtered by working directory.
- **History** (`wardex history`, MCP `history`): a session's conversation,
  normalized, paged and redacted, read from the native transcript on every
  call.
- **Adopt** (`wardex adopt`, MCP `adopt`): link an external session to a new
  Wardex session record, so `send` / `resume` continue it in a Wardex process
  under the same native id.
- **Fork** (`wardex adopt --fork`, `wardex run --fork-from`): start a new
  native session branched from the external one; the original is never
  written. This is the only safe way to continue a session that is still open
  elsewhere.

The feature is **off by default** (see [Configuration](#configuration)).

## Ground rules

- Wardex **reads** the CLIs' stores under `~/.claude*` and `~/.codex/`; it
  never writes, locks, moves or deletes anything there. When a session is
  continued or forked, the CLI itself writes its transcript, exactly as it
  does for sessions Wardex starts.
- History is **not copied** into the Wardex database. `history` reads the
  native transcript every time, so it also shows the turns a Wardex process
  added later (they go to the same file). `tail` keeps showing Wardex events.
- Detecting a live session is a data integrity guard, not a policy layer: two
  writers appending to the same transcript corrupt its branches. Nothing is
  ever signalled, traced, locked or sent an RPC; Wardex only reads `/proc` and
  the CLIs' own registry files, and only for the daemon's own user.
- Only ids are accepted over the API (never paths). A resolved transcript must
  be a regular file (not a symlink) under the configured root, owned by the
  daemon's user, and smaller than `external.max_file_mb`.

## Storage formats

Both formats are undocumented internal formats of the CLIs and change between
versions. The readers skip unknown line types and missing fields. A version
outside the tested range (see [Versions](#versions)) adds the warning
`format_version_untested`; it is never refused.

### Claude Code

Transcripts: `<root>/projects/<encoded-cwd>/<session-uuid>.jsonl`, where the
root is `external.claude_home`, else `$CLAUDE_CONFIG_DIR`, else `~/.claude`.

- The directory name is the session's starting cwd with every
  non-alphanumeric character replaced by `-`. The encoding is lossy (`/a-b`
  and `/a/b` collide), so Wardex never derives the cwd from it: the session
  cwd is the first `cwd` field in the file (tools may change it later).
- The file name is the session id. Side directories (`<uuid>/subagents/…`,
  `tool-results/`, `memory/`) are not sessions and are ignored.
- Every line is one JSON object with a `type`:

| `type` | Used for |
|---|---|
| `user` | `prompt` (plain text or `text` blocks), tool results (`tool_result` blocks, matched by `tool_use_id`). Lines with `isMeta`, `isSidechain` or `isCompactSummary` are skipped. |
| `assistant` | `text` blocks → `message`; `tool_use` blocks → `tool_call`. `thinking` is skipped. |
| `system` | `compact_boundary` → `compaction`; bridges the parent chain with `logicalParentUuid`. Other subtypes are skipped. |
| `last-prompt` | `leafUuid`: a node on the active branch. |
| `custom-title`, `ai-title` | Title (`customTitle` wins over `aiTitle`). |
| `continued-in` | `continuedInSessionId`: the conversation continued under another id. |
| everything else | skipped (`attachment`, `file-history-*`, `queue-operation`, `bridge-session`, `api-request*`, …) |

- **Branches.** Rewinds and retries give a `parentUuid` several children, so
  reading the file top to bottom mixes abandoned branches in. The active
  branch is found by starting at the last `last-prompt.leafUuid`, following
  the child whose subtree was written last down to a leaf, then walking
  `parentUuid` (or `logicalParentUuid` across a compaction) back to the root.
  Without a `last-prompt` line the newest message is the leaf (warning
  `no_leaf_marker`).
- **Parallel tool calls** are written one `tool_use` line after another, and
  each `tool_result` hangs off its own call, so all results but the last are
  off the active branch. Results are therefore matched to calls by
  `tool_use_id` across the whole file.
- A half-written last line (the CLI is still appending) is skipped with the
  warning `partial_last_line`.

Live registry: `<root>/sessions/<pid>.json` with `pid`, `sessionId`, `cwd`,
`procStart`, … `procStart` equals field 22 (`starttime`) of
`/proc/<pid>/stat`, which rules out a reused PID. `*.key` files in that
directory are never read. Live `claude` processes do not keep the transcript
open, so file descriptors and locks cannot be used.

Resume: `claude --resume <id>` (same id). Fork:
`claude --resume <id> --fork-session --session-id <new-uuid>`; Claude accepts
`--session-id` together with `--resume` only with `--fork-session`, which lets
Wardex know the new id before the process starts.

### Codex

Rollouts: `<root>/sessions/YYYY/MM/DD/rollout-<time>-<thread-uuid>.jsonl`,
where the root is `external.codex_home`, else `$CODEX_HOME`, else `~/.codex`.
Older rollouts are compressed to `.jsonl.zst` (read with the pure-Rust
`ruzstd`); archived ones live under `<root>/archived_sessions/` and are listed
only on request.

- Every line is `{timestamp, type, payload}` (0.16x adds `ordinal`). The first
  line is always `session_meta` (`id`, `cwd`, `cli_version`, `originator`,
  `source`, `forked_from_id`, …). A forked rollout has a second
  `session_meta` copied from its source, so the session id is the file name's
  uuid, which equals the first `session_meta.id`.
- `source` is a string (`cli`, `exec`, `vscode`, …) or an object
  (`{"subagent": …}`); subagent threads are listed only with
  `include_subagents`.
- `originator` is `wardex` for threads Wardex started. Such a thread that is
  not in this daemon's database (another or a test daemon) is listed with a
  warning.
- Titles come from `<root>/session_index.jsonl` (`id`, `thread_name`).
- History comes from `event_msg` lines with `payload.type = item_completed`:

| `item.type` | Used for |
|---|---|
| `UserMessage` | `prompt` (`content[].text`) |
| `AgentMessage` | `message` (`content[].text`) |
| `CommandExecution` | `tool_call` `shell` (`command`, `exit_code`, `status`, `aggregated_output`) |
| `FileChange` | `tool_call` `file_change` (`changes`: path → change) |
| `McpToolCall` | `tool_call` `mcp:<server>/<tool>` (`arguments`, `result`, `status`) |
| `ContextCompaction` | `compaction` |
| everything else | skipped (`Reasoning` is encrypted, developer messages, `turn_context`, `token_count`, …) |

- Codex also keeps SQLite state (`state_*.sqlite`, `thread_history_*.sqlite`).
  Wardex does not read it: internal schema, WAL locks and versioned file names.
- Live `codex` processes do not keep the rollout open either, and a shared
  local app-server daemon can host TUI threads without the thread id in any
  process arguments.

Resume: `thread/resume {threadId, excludeTurns: true}` (as for Wardex's own
sessions). Fork: `thread/fork {threadId, cwd, approvalPolicy, sandbox, model?,
excludeTurns: true}`; the result's `thread.id` is the new id.

## Liveness

Each listed session gets a `liveness`:

| Value | Meaning |
|---|---|
| `live` | Hard evidence that a CLI process has the session open (`evidence`: `registry`, `cmdline`, `cwd_recent`), with its `pid`. |
| `recent` | No process evidence, but the transcript was written within `external.quiet_secs` (default 120 s), or a Codex process runs in the session's cwd. |
| `inactive` | Neither. |
| `unknown` | `/proc` could not be read (e.g. `hidepid`). |

Evidence, in order:

- Claude: a registry entry with this `sessionId` whose `pid` is alive and
  whose `procStart` matches `/proc/<pid>/stat` (`registry`); a `claude`
  process with `--resume <id>`, `-r <id>` or `--session-id <id>` in its
  arguments (`cmdline`); the transcript's mtime (`recent`).
- Codex: a `codex` process with the thread id in its arguments (`cmdline`); a
  `codex` process whose cwd is the session cwd **and** a rollout written within
  `quiet_secs` (`cwd_recent`); only one of the two is `recent`.
- Processes of this daemon's own live sessions (and their children) never
  count.

Admission rules:

| Liveness | `adopt` (same id) | `adopt --fork` | `history` |
|---|---|---|---|
| `live` | refused (`conflict`, hint: `--fork`) | allowed (the source is only read) | allowed, warning `live` |
| `recent` / `unknown` | refused unless `--force` (CLI only) | allowed | allowed |
| `inactive` | allowed | allowed | allowed |

An adopted session is probed again on every `resume` and implicit `send`
resume, because the user may have opened it in a terminal in the meantime:
registry or command-line evidence refuses the resume and nothing is started.
The cwd heuristic alone does not block a resume (another Codex thread may be
open in the same directory). The reverse (opening a session in a terminal
while Wardex runs it) cannot be prevented; do not do it.

## History

`history` returns a page of normalized items; raw lines are never returned.

```json
{"session": {…}, "items": [
   {"seq": 12, "turn": 4, "ts_ms": 1760000000000, "kind": "tool_call",
    "tool": {"name": "Bash", "input_summary": "cargo test", "status": "completed",
             "result_summary": "test result: ok"},
    "ref": "<claude uuid | codex line number>"}],
 "next_before": "11", "truncated": false, "warnings": []}
```

- `kind`: `prompt`, `message` (both with `text`), `tool_call` (with `tool`),
  `compaction`. `turn` counts prompts.
- Paging: the last `turns` turns (default 20) before the `before` cursor;
  pass `next_before` to get the previous page. `max_bytes` (default 64 KiB)
  drops the oldest items of the page first and sets `truncated`.
- `tools`: `none` (no tool items), `summary` (default: name, input and result
  summaries), `full` (also the input and output, each capped at 256 KiB).
- The CLI prints `text`, `markdown` (a context summary that can be handed to
  another agent as a prompt, no extra LLM call) or `json`.

### Never returned

The normalizer builds items from an allowlist of fields, so these never reach
the output: Claude `attachment` lines (including `environment` and
`credential_org`), `bridge-session`, `api-request*`, `thinking` blocks; Codex
`session_meta.creator_user_id` / `creator_account_id` / `base_instructions`,
`reasoning.encrypted_content` and developer messages. Credential files
(`.credentials.json`, `~/.codex/auth.json`, registry `*.key`) are never
opened.

### Redaction

With `external.redact = true` (default), text is masked on a best-effort basis
before it leaves the daemon: well-known token formats, `KEY=` / `TOKEN=` /
`SECRET=` / `PASSWORD=` style assignments, `Authorization:` headers and PEM
private key blocks. **This is not a guarantee.** A transcript can contain
anything the user pasted or a tool printed; treat history as sensitive.

## Adopt

```text
wardex adopt <ID> [--driver claude|codex] [--fork] [--agent A] [--task T]
                  [--model M] [--reasoning R] [--readonly]
                  [--message TEXT | --prompt-file F] [--force] [--json]
```

1. The id is resolved (`claude:<uuid>`, `codex:<uuid>`, a full uuid or a
   unique prefix of at least 8 characters).
2. Without `--fork`, a session this daemon already manages returns that
   Wardex session (idempotent), and the liveness rules above apply.
3. A session record is created in state `stopped` with `ended_reason`
   `adopted` (no Wardex process has run yet) and `origin` `adopted` or
   `forked`, and an `adopted` event is recorded. The record's cwd is the
   session's starting cwd.
4. With `--message`, the session is resumed with it at once. Otherwise the
   next `send` resumes it; for `--fork` that is when the native fork happens.

`origin` stays on the record (`wardex list --origin adopted|forked|wardex`).
After the first resume the session behaves like any other: idle shutdown,
memory protection and `ended_reason` apply as usual.

**Permissions.** Wardex runs Claude with `bypassPermissions` and Codex with
`danger-full-access` unless `--readonly` is given. A session the user started
in a restricted mode continues with full permissions after adopt; the response
says so. Use `--readonly` to keep it read-only.

`wardex run --fork-from <ID> --prompt …` is a shortcut for
`adopt --fork --message …`. Over MCP, use `adopt` with `fork` and `message`
(the `run` tool keeps `cwd` required).

A fork record has no native id of its own until it first runs: it keeps the
source in `forked_from`, `history` shows the source meanwhile, and the first
`send` (or the `--message`) starts the native fork. Every later resume
continues the fork, never the source. Each `adopt --fork` creates a new
session.

## Configuration

```toml
[external]
enabled = false        # discovery, history and adopt over the Unix socket
mcp = false            # also expose them over MCP (HTTP and stdio); needs enabled
# claude_home = "~/.claude"   # default: $CLAUDE_CONFIG_DIR, else ~/.claude
# codex_home = "~/.codex"     # default: $CODEX_HOME, else ~/.codex
quiet_secs = 120       # a transcript written this recently counts as `recent`
cwd_allow = []         # globs; only sessions whose cwd matches one are visible
max_file_mb = 128      # larger transcripts are refused
redact = true          # best-effort secret masking in history
```

Both switches are off by default. Wardex has no roles: every MCP client sees
everything, and MCP may be reachable through a tunnel, so a user's personal
terminal history reaches that surface only with explicit consent
(`mcp = true`). `--force` exists only in the CLI.

## Versions

Tested: Claude Code 2.1.302 and Codex 0.163.0. Versions seen in transcripts
and treated as tested: Claude 2.1.295 – 2.1.302, Codex 0.162.1 – 0.163.0.
Tests use synthetic fixtures under
`crates/wardex-drivers/tests/fixtures/{claude,codex}/store/`; no real
transcript is ever committed.
