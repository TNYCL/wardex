# Writes the synthetic Claude Code store fixtures (no real content):
#   python3 tools/fixtures/store_claude.py crates/wardex-drivers/tests/fixtures/claude/store
import json, os, sys
root = sys.argv[1]
S1 = "11111111-1111-4111-8111-111111111111"
S2 = "22222222-2222-4222-8222-222222222222"
S3 = "33333333-3333-4333-8333-333333333333"
def common(sid, cwd, version="2.1.300", entry="cli"):
    return {"sessionId": sid, "cwd": cwd, "version": version, "entrypoint": entry, "gitBranch": "main", "userType": "external"}
def ts(n): return "2026-10-01T10:%02d:00.000Z" % n
def user(sid, uuid, parent, content, n, **kw):
    d = {"type": "user", "uuid": uuid, "parentUuid": parent, "timestamp": ts(n), "isSidechain": False,
         "message": {"role": "user", "content": content}}
    d.update(common(sid, kw.pop("cwd", "/work/app"), kw.pop("version", "2.1.300"), kw.pop("entry", "cli"))); d.update(kw); return d
def asst(sid, uuid, parent, block, n, **kw):
    d = {"type": "assistant", "uuid": uuid, "parentUuid": parent, "timestamp": ts(n), "isSidechain": False,
         "requestId": "req_synthetic", "message": {"id": "msg_" + uuid, "role": "assistant", "model": "claude-synthetic",
         "content": [block], "stop_reason": None, "usage": {"input_tokens": 1, "output_tokens": 1}}}
    d.update(common(sid, kw.pop("cwd", "/work/app"), kw.pop("version", "2.1.300"))); d.update(kw); return d
def text(t): return {"type": "text", "text": t}
s1 = [
    {"type": "file-history-snapshot", "messageId": "snap-1", "snapshot": {"trackedFileBackups": {}}, "isSnapshotUpdate": False},
    user(S1, "u1", None, "Add a health check endpoint", 0),
    {"type": "attachment", "uuid": "a1", "parentUuid": "u1", "timestamp": ts(0), "isSidechain": False,
     "attachment": {"type": "credential_org", "orgName": "SYNTHETIC-ORG-NAME"}, **common(S1, "/work/app")},
    asst(S1, "m1", "a1", {"type": "thinking", "thinking": "SYNTHETIC-THINKING", "signature": "sig"}, 1),
    asst(S1, "m2", "m1", text("I'll look at the router first."), 1),
    asst(S1, "m3", "m2", {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "grep -rn health src/", "description": "Find health code"}}, 1),
    user(S1, "r1", "m3", [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "src/router.rs:12: // TODO health"}], 2,
         toolUseResult={"stdout": "src/router.rs:12: // TODO health", "stderr": ""}, sourceToolUseID="toolu_1"),
    asst(S1, "m4", "r1", text("Old answer that was rewound."), 3),
    asst(S1, "m4b", "r1", text("Added GET /healthz returning 200."), 4),
    {"type": "system", "subtype": "turn_duration", "uuid": "s1", "parentUuid": "m4b", "timestamp": ts(4), "durationMs": 1234, "isSidechain": False, **common(S1, "/work/app")},
    user(S1, "u2", "s1", "<local-command-caveat>Synthetic meta message</local-command-caveat>", 5, isMeta=True),
    user(S1, "u3", "u2", "Now add a test", 6),
    asst(S1, "sc1", "u3", text("SIDECHAIN-TEXT"), 6, isSidechain=True),
    {"type": "system", "subtype": "compact_boundary", "uuid": "c1", "parentUuid": None, "logicalParentUuid": "u3", "timestamp": ts(7),
     "content": "Conversation compacted", "compactMetadata": {"trigger": "auto", "preTokens": 1000}, "isSidechain": False, **common(S1, "/work/app")},
    user(S1, "cs1", "c1", "SYNTHETIC-COMPACT-SUMMARY", 7, isCompactSummary=True),
    user(S1, "u4", "cs1", [text("Run the tests")], 8),
    {"type": "last-prompt", "lastPrompt": "Run the tests", "leafUuid": "u4", "sessionId": S1},
    asst(S1, "m5", "u4", {"type": "tool_use", "id": "toolu_2", "name": "Bash", "input": {"command": "cargo test"}}, 9),
    user(S1, "r2", "m5", [{"type": "tool_result", "tool_use_id": "toolu_2", "is_error": True, "content": [text("test failed: expected 200")]}], 10),
    asst(S1, "m6", "r2", text("The test fails; it printed API_KEY=xxxxxxxxxxxxxxxx on the way."), 11),
    {"type": "custom-title", "customTitle": "Health check", "sessionId": S1},
    {"type": "ai-title", "aiTitle": "Add health endpoint", "sessionId": S1},
    {"type": "bridge-session", "ownerAccountUuid": "SYNTHETIC-OWNER", "sessionId": S1},
]
s2 = [
    user(S2, "v1", None, "Explain the build", 0, cwd="/work/app-b", version="2.1.999", entry="claude-desktop"),
    asst(S2, "w1", "v1", text("It uses cargo."), 1, cwd="/work/app-b/sub", version="2.1.999"),
    {"type": "continued-in", "continuedInSessionId": "44444444-4444-4444-8444-444444444444", "sessionId": S2},
]
s3 = [
    user(S3, "x1", None, "What is in this directory?", 0, cwd="/work/app/b"),
    asst(S3, "y1", "x1", text("A Makefile."), 1, cwd="/work/app/b"),
    {"type": "ai-title", "aiTitle": "Directory listing", "sessionId": S3},
]
def write(path, lines, tail=""):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        for l in lines: f.write(json.dumps(l, separators=(",", ":")) + "\n")
        f.write(tail)
p = os.path.join(root, "projects")
write(f"{p}/-work-app/{S1}.jsonl", s1)
write(f"{p}/-work-app-b/{S2}.jsonl", s2, '{"type":"user","uuid":"v2","parentUu')
write(f"{p}/-work-app-b/{S3}.jsonl", s3)
write(f"{p}/-work-app/{S1}/subagents/agent-a1b2c3.jsonl", [user(S1, "sa1", None, "SUBAGENT-PROMPT", 0, isSidechain=True)])
write(f"{p}/-work-app/notes.jsonl", [{"type": "note", "cwd": "/work/app"}])
os.makedirs(f"{p}/-work-app/memory", exist_ok=True)
open(f"{p}/-work-app/memory/MEMORY.md", "w").write("# Synthetic memory\n")
