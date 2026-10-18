"""Sanitize a recorded agent CLI transcript into a committable fixture.

Usage: python3 -I sanitize.py <in.jsonl> <out.jsonl> <recording-dir>

Input lines are {"t", "dir", "msg"} records written by record_claude.py /
record_codex.py. The sanitizer:
- replaces the recording directory, the home directory and their
  dash-mangled forms with fixed placeholders,
- drops user-specific inventory (skills, plugins, agents, MCP servers, slash
  commands) from Claude's init event,
- drops Codex notifications that only describe the local account or MCP setup,
- shortens opaque thinking signatures and replaces API ids with placeholders,
- drops stderr lines.

Always grep the output for anything personal before committing it.
"""
import json
import os
import re
import sys

src, dst, rec_dir = sys.argv[1:]
home = os.path.expanduser("~")


def mangle(path):
    return re.sub(r"[^A-Za-z0-9]", "-", path)


REPLACEMENTS = [
    (rec_dir, "/work"),
    (mangle(rec_dir), "-work"),
    (home, "/home/user"),
    (mangle(home), "-home-user"),
]
UUID_IN_PATH = re.compile(r"/tmp/claude-\d+/[^\"\s]*")
SOCKET = re.compile(r"/run/user/\d+/[^\"\s]*")

CODEX_DROP = (
    "remoteControl/",
    "account/",
    "mcpServer/startupStatus/updated",
    "turn/moderationMetadata",
    "skills/changed",
)
CLAUDE_INIT_EMPTY = ("mcp_servers", "slash_commands", "terminal_slash_commands", "agents", "skills", "plugins")


# Opaque API ids look like secrets to scanners. Replace them with stable
# placeholders, keeping equal ids equal so events still correlate.
API_ID = re.compile(r"\b(msg|req|toolu|call)_[A-Za-z0-9]{12,}\b")
api_ids = {}


def placeholder(m):
    key = m.group(0)
    if key not in api_ids:
        api_ids[key] = "%s_%012d" % (m.group(1), len(api_ids) + 1)
    return api_ids[key]


def scrub_str(s):
    for a, b in REPLACEMENTS:
        s = s.replace(a, b)
    s = UUID_IN_PATH.sub("/tmp/claude/redacted", s)
    s = SOCKET.sub("/run/user/redacted.sock", s)
    s = API_ID.sub(placeholder, s)
    return s


def scrub(v):
    if isinstance(v, str):
        return scrub_str(v)
    if isinstance(v, list):
        return [scrub(x) for x in v]
    if isinstance(v, dict):
        out = {}
        for k, x in v.items():
            if k == "signature" and isinstance(x, str):
                x = "redacted"
            out[k] = scrub(x)
        return out
    return v


def keep(rec):
    if rec["dir"] == "stderr":
        return False
    m = rec["msg"]
    if isinstance(m, dict) and str(m.get("method", "")).startswith(CODEX_DROP):
        return False
    return True


def clean_init(m):
    if isinstance(m, dict) and m.get("type") == "system" and m.get("subtype") == "init":
        m = dict(m)
        for k in CLAUDE_INIT_EMPTY:
            if k in m:
                m[k] = []
        m["tools"] = [t for t in m.get("tools", []) if not t.startswith("mcp__")]
        m.pop("memory_paths", None)
    return m


with open(src) as f, open(dst, "w") as out:
    for line in f:
        rec = json.loads(line)
        if "dir" not in rec:  # raw stdout line from a plain recording
            rec = {"t": None, "dir": "out", "msg": rec}
        if not keep(rec):
            continue
        rec["msg"] = scrub(clean_init(rec["msg"]))
        out.write(json.dumps(rec, ensure_ascii=False) + "\n")
