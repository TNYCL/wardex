# Writes the synthetic Codex store fixtures (no real content):
#   python3 tools/fixtures/store_codex.py crates/wardex-drivers/tests/fixtures/codex/store
import json, os, sys

root = sys.argv[1]
A = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a01"  # tui session with every item type
B = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a02"  # fork of A
C = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a03"  # started by a Wardex daemon
D = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a04"  # subagent thread
E = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a05"  # archived
G = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a07"  # legacy: response items only


def ts(n):
    return "2026-10-01T10:%02d:00.000Z" % n


def meta(sid, n, cwd="/work/svc", originator="codex-tui", source="cli", version="0.163.0", **kw):
    p = {"id": sid, "session_id": sid, "timestamp": ts(n), "cwd": cwd, "originator": originator,
         "cli_version": version, "source": source, "history_mode": "paginated", "model_provider": "openai",
         "creator_user_id": "SYNTHETIC-CREATOR-USER", "creator_account_id": "SYNTHETIC-CREATOR-ACCOUNT",
         "base_instructions": {"text": "SYNTHETIC-BASE-INSTRUCTIONS"},
         "git": {"branch": "main", "commit_hash": "0000000", "repository_url": "https://example.invalid/repo.git"}}
    p.update(kw)
    return {"timestamp": ts(n), "type": "session_meta", "payload": p}


class Rollout:
    def __init__(self):
        self.lines = []

    def add(self, n, type_, payload):
        self.lines.append({"ordinal": len(self.lines), "timestamp": ts(n), "type": type_, "payload": payload})

    def item(self, n, item, turn="turn-1"):
        self.add(n, "event_msg", {"type": "item_completed", "thread_id": "t", "turn_id": turn,
                                  "started_at_ms": 0, "completed_at_ms": 1790848800000 + n * 60000, "item": item})


def user(text, id_):
    return {"type": "UserMessage", "id": id_, "client_id": "c", "content": [{"type": "text", "text": text, "text_elements": []}]}


def agent(text, id_, phase="final_answer"):
    return {"type": "AgentMessage", "id": id_, "phase": phase, "content": [{"type": "Text", "text": text}]}


def write(path, lines, tail=""):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        for l in lines:
            f.write(json.dumps(l, separators=(",", ":")) + "\n")
        f.write(tail)


day = os.path.join(root, "sessions", "2026", "10", "01")

a = Rollout()
a.lines.append(meta(A, 0))
a.add(0, "turn_context", {"turn_id": "turn-1", "cwd": "/work/svc", "model": "gpt-synthetic", "approval_policy": "never"})
a.add(0, "response_item", {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "SYNTHETIC-DEVELOPER-MESSAGE"}]})
a.add(0, "event_msg", {"type": "task_started", "turn_id": "turn-1"})
a.item(0, user("Fix the flaky login test", "i1"))
a.add(1, "response_item", {"type": "reasoning", "summary": [], "encrypted_content": "SYNTHETIC-ENCRYPTED"})
a.item(1, {"type": "Reasoning", "id": "i2", "summary_text": ["SYNTHETIC-REASONING"], "raw_content": []})
a.item(1, agent("Looking at the test first.", "i3", "commentary"))
a.item(2, {"type": "CommandExecution", "id": "i4", "command": ["/bin/bash", "-lc", "cargo test login"], "cwd": "/work/svc",
           "status": "failed", "exit_code": 101, "aggregated_output": "test login ... FAILED\nTOKEN=xxxxxxxxxxxxxxxx",
           "duration": {"secs": 1, "nanos": 0}, "parsed_cmd": [], "source": "agent"})
a.item(3, {"type": "FileChange", "id": "i5", "status": "completed",
           "changes": {"/work/svc/tests/login.rs": {"type": "update", "unified_diff": "@@ -1 +1 @@\n-sleep(1)\n+wait_for_ready()", "move_path": None}}})
a.item(4, {"type": "McpToolCall", "id": "i6", "server": "docs", "tool": "search", "status": "completed",
           "arguments": {"query": "retry helper"}, "result": {"content": [{"type": "text", "text": "found retry::wait"}], "isError": False}})
a.item(5, agent("Replaced the sleep with a readiness wait.", "i7"))
a.add(5, "event_msg", {"type": "task_complete", "turn_id": "turn-1"})
a.add(5, "event_msg", {"type": "token_count", "info": None})
a.item(6, {"type": "ContextCompaction", "id": "i8"}, "turn-2")
a.item(7, user("Run it ten times", "i9"), "turn-2")
a.item(8, agent("All ten runs passed.", "i10"), "turn-2")
a.add(8, "world_state", {"anything": "ignored"})
write(os.path.join(day, f"rollout-2026-10-01T10-00-00-{A}.jsonl"), a.lines)

b = Rollout()
b.lines.append(meta(B, 10, forked_from_id=A))
b.lines.append(meta(A, 0))
b.item(10, user("Fix the flaky login test", "j1"))
b.item(11, agent("Replaced the sleep with a readiness wait.", "j2"))
b.item(12, user("Also add a regression test", "j3"))
write(os.path.join(day, f"rollout-2026-10-01T10-10-00-{B}.jsonl"), b.lines,
      '{"ordinal":4,"timestamp":"2026-10-01T10:13:00.000Z","type":"event_msg","payl')

c = Rollout()
c.lines.append(meta(C, 20, cwd="/work/svc/.worktree/fix", originator="wardex", source="exec", version="0.170.0"))
c.item(20, user("Started by another daemon", "k1"))
write(os.path.join(day, f"rollout-2026-10-01T10-20-00-{C}.jsonl"), c.lines)

d = Rollout()
d.lines.append(meta(D, 30, source={"subagent": {"thread_spawn": {"parent_thread_id": A, "depth": 1}}}, parent_thread_id=A))
d.item(30, user("Subagent task", "l1"))
write(os.path.join(day, f"rollout-2026-10-01T10-30-00-{D}.jsonl"), d.lines)

e = Rollout()
e.lines.append(meta(E, 40, cwd="/work/old"))
e.item(40, user("Archived question", "m1"))
write(os.path.join(root, "archived_sessions", f"rollout-2026-09-01T10-40-00-{E}.jsonl"), e.lines)

g = Rollout()
g.lines.append(meta(G, 50, cwd="/work/legacy", version="0.162.1"))
g.add(50, "response_item", {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>\n  <cwd>/work/legacy</cwd>\n</environment_context>"}]})
g.add(50, "response_item", {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "List the files"}]})
g.add(51, "response_item", {"type": "function_call", "name": "shell", "call_id": "call_1", "arguments": "{\"command\":[\"ls\"]}"})
g.add(51, "response_item", {"type": "function_call_output", "call_id": "call_1", "output": "Cargo.toml\nsrc"})
g.add(52, "response_item", {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Two entries."}]})
write(os.path.join(day, f"rollout-2026-10-01T10-50-00-{G}.jsonl"), g.lines)

write(os.path.join(root, "session_index.jsonl"), [
    {"id": A, "thread_name": "Fix flaky test", "updated_at": ts(1)},
    {"id": B, "thread_name": "Regression test", "updated_at": ts(12)},
    {"id": A, "thread_name": "Fix flaky login test", "updated_at": ts(8)},
])
os.makedirs(os.path.join(root, "sessions", "2026", "10", "01"), exist_ok=True)
open(os.path.join(day, "not-a-rollout.jsonl"), "w").write("{}\n")
