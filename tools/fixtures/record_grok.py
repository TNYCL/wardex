"""Record a Grok Build ACP session (`grok agent stdio`) for driver fixtures.

Usage: python3 -I record_grok.py <scenario> <out.jsonl> [session_id]
Env: GROK_BIN (default: grok), RECORD_MODEL, RECORD_EFFORT, RECORD_ARGS
(extra `grok agent` arguments, space separated), RECORD_YOLO and
RECORD_AUTO (`0`/`1`: `_meta.yoloMode` / `_meta.autoMode` on `session/new`;
unset: not sent), RECORD_SET (`id=value,...`: `session/set_config_option`
calls before the first prompt, e.g. `reasoning_effort=low`), RECORD_ANSWER
(`_x.ai/ask_user_question` result: `pick:<label>` answers every question with
that option label, keyed by question text; anything else is sent as JSON;
unset: method-not-found error). Run it in a scratch
directory, then pass the result through sanitize.py.

Scenarios: basic, tool, approval_deny, approval_allow, cancel, question,
load, resume.
"""
import json
import os
import queue
import subprocess
import sys
import threading
import time

scenario, out_path, *rest = sys.argv[1:]
t0 = time.monotonic()
log = open(out_path, "w")
lock = threading.Lock()


def rec(direction, obj):
    with lock:
        log.write(json.dumps({"t": round(time.monotonic() - t0, 3), "dir": direction, "msg": obj}) + "\n")
        log.flush()


args = [os.environ.get("GROK_BIN", "grok"), "agent", "--no-leader"]
if os.environ.get("RECORD_MODEL"):
    args += ["--model", os.environ["RECORD_MODEL"]]
if os.environ.get("RECORD_EFFORT"):
    args += ["--reasoning-effort", os.environ["RECORD_EFFORT"]]
args += os.environ.get("RECORD_ARGS", "").split()
args += ["stdio"]
p = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
lines = queue.Queue()
threading.Thread(target=lambda: [lines.put(l) for l in p.stdout] + [lines.put(None)], daemon=True).start()
threading.Thread(target=lambda: [rec("stderr", l.rstrip("\n")) for l in p.stderr], daemon=True).start()

next_id = 0
pending = {}


def send(obj):
    obj = {"jsonrpc": "2.0", **obj}
    rec("in", obj)
    p.stdin.write(json.dumps(obj) + "\n")
    p.stdin.flush()


def request(method, params):
    global next_id
    next_id += 1
    pending[next_id] = method
    send({"id": next_id, "method": method, "params": params})
    return next_id


PROMPTS = {
    "basic": ["Reply with exactly: pong", "Reply with exactly: ping"],
    "tool": ["Run the shell command `ls -a` and reply with the number of entries."],
    "approval_deny": ["Run exactly this shell command: `git push --force origin main`, then reply done."],
    "approval_allow": ["Run exactly this shell command: `git push --force origin main`, then reply done."],
    "cancel": ["Run the shell command `sleep 60 && echo done` and reply with its output."],
    "question": ["Before doing anything else, use your tool for asking the user a question to ask me "
                 "which color I prefer, with the options red and blue. Then reply with only the chosen color."],
    "load": ["What word did you reply with in the previous turn? Reply with only that word."],
    "resume": ["What word did you reply with in the previous turn? Reply with only that word."],
}
prompts = list(PROMPTS[scenario])
cwd = os.getcwd()

request("initialize", {
    "protocolVersion": 1,
    "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False}, "terminal": False},
    "clientInfo": {"name": "wardex-recorder", "version": "0.0.1"},
})

session_id = None
cancel_at = None
deadline = time.monotonic() + 600
closed = False
while time.monotonic() < deadline:
    if cancel_at and time.monotonic() >= cancel_at:
        cancel_at = None
        send({"method": "session/cancel", "params": {"sessionId": session_id}})
    try:
        line = lines.get(timeout=0.5)
    except queue.Empty:
        continue
    if line is None:
        break
    line = line.rstrip("\n")
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        rec("out", {"raw": line})
        continue
    rec("out", msg)
    if "method" in msg and "id" in msg:
        method = msg["method"]
        if method == "session/request_permission":
            options = msg["params"].get("options", [])
            want = "allow_once" if scenario == "approval_allow" else "reject_once"
            choice = next((o for o in options if o.get("kind") == want), None)
            if choice is None:
                result = {"outcome": {"outcome": "cancelled"}}
            else:
                result = {"outcome": {"outcome": "selected", "optionId": choice["optionId"]}}
            send({"id": msg["id"], "result": result})
        elif method == "_x.ai/ask_user_question" and os.environ.get("RECORD_ANSWER"):
            answer = os.environ["RECORD_ANSWER"]
            if answer.startswith("pick:"):
                questions = msg["params"].get("questions", [])
                result = {"outcome": "accepted", "answers": {q["question"]: answer[5:] for q in questions}}
            else:
                result = json.loads(answer)
            send({"id": msg["id"], "result": result})
        else:
            send({"id": msg["id"], "error": {"code": -32601, "message": "method not found"}})
        continue
    if "method" in msg:
        if scenario == "cancel" and cancel_at is None and session_id and msg["method"] == "session/update":
            if msg["params"].get("update", {}).get("sessionUpdate") == "tool_call":
                cancel_at = time.monotonic() + 2
        continue
    method = pending.pop(msg.get("id"), None)
    if method == "initialize":
        params = {"cwd": cwd, "mcpServers": []}
        meta = {}
        for env, key in (("RECORD_YOLO", "yoloMode"), ("RECORD_AUTO", "autoMode")):
            if os.environ.get(env) in ("0", "1"):
                meta[key] = os.environ[env] == "1"
        if meta:
            params["_meta"] = meta
        if scenario in ("load", "resume"):
            session_id = rest[0]
            request(f"session/{scenario}", {"sessionId": session_id, **params})
        else:
            request("session/new", params)
    elif method in ("session/new", "session/load", "session/resume"):
        if "error" in msg:
            break
        session_id = session_id or msg["result"]["sessionId"]
        settings = [kv.split("=", 1) for kv in os.environ.get("RECORD_SET", "").split(",") if kv]
        if settings:
            config_id, value = settings.pop(0)
            request("session/set_config_option", {"sessionId": session_id, "configId": config_id, "value": value})
        else:
            request("session/prompt", {"sessionId": session_id, "prompt": [{"type": "text", "text": prompts.pop(0)}]})
    elif method == "session/set_config_option":
        if settings:
            config_id, value = settings.pop(0)
            request("session/set_config_option", {"sessionId": session_id, "configId": config_id, "value": value})
        else:
            request("session/prompt", {"sessionId": session_id, "prompt": [{"type": "text", "text": prompts.pop(0)}]})
    elif method == "session/prompt":
        if prompts:
            request("session/prompt", {"sessionId": session_id, "prompt": [{"type": "text", "text": prompts.pop(0)}]})
        else:
            p.stdin.close()
            closed = True
if not closed:
    p.stdin.close()
try:
    code = p.wait(timeout=30)
except subprocess.TimeoutExpired:
    p.kill()
    code = p.wait()
rec("exit", code)
print(f"{scenario}: exit {code}, session {session_id}")
