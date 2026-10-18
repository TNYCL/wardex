"""Record a Claude Code stream-json session for driver fixtures.

Usage: python3 -I record_claude.py <scenario> <out.jsonl> [extra claude args...]
Model: $RECORD_MODEL (default: haiku). Run it in a scratch directory, then
pass the result through sanitize.py. Scenario-specific extra args:
  partial:   --include-partial-messages
  resume:    --resume <session_id from the multi scenario>
  plan_deny: --permission-mode plan
Every line read from or written to claude is logged with a direction marker
and a relative timestamp, so the transcript doubles as a fixture.
"""
import json
import os
import subprocess
import sys
import threading
import time

scenario, out_path, *extra = sys.argv[1:]
t0 = time.monotonic()
log = open(out_path, "w")
lock = threading.Lock()


def rec(direction, obj):
    with lock:
        log.write(json.dumps({"t": round(time.monotonic() - t0, 3), "dir": direction, "msg": obj}) + "\n")
        log.flush()


args = [
    "claude", "-p", "--input-format", "stream-json", "--output-format", "stream-json",
    "--verbose", "--permission-mode", "bypassPermissions", "--model", os.environ.get("RECORD_MODEL", "haiku"),
    "--permission-prompt-tool", "stdio", *extra,
]
p = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)


def send(obj):
    rec("in", obj)
    p.stdin.write(json.dumps(obj) + "\n")
    p.stdin.flush()


def user(text):
    send({"type": "user", "message": {"role": "user", "content": text}})


def control_ok(req_id, response):
    send({"type": "control_response", "response": {"subtype": "success", "request_id": req_id, "response": response}})


def stderr_pump():
    for line in p.stderr:
        rec("stderr", line.rstrip("\n"))


threading.Thread(target=stderr_pump, daemon=True).start()

ANSWER_DELAY = float({"question": 20, "question_deny": 2}.get(scenario, 0))
results = 0

PROMPTS = {
    "question": "Use the AskUserQuestion tool to ask me which color I prefer, with options red and blue. Then reply with only the chosen color.",
    "question_deny": "Use the AskUserQuestion tool to ask me which color I prefer, with options red and blue. If it fails, reply with exactly: no answer.",
    "tool": "Run the shell command `ls` and then reply with the number of files.",
    "interrupt": "Run the shell command `sleep 60 && echo done` and then reply with its output.",
    "partial": "Reply with exactly: pong",
    "multi": "Reply with exactly: one",
    "resume": "What word did you reply with in the previous turn? Reply with only that word.",
    "plan_deny": "Create a file named plan_test.txt containing hi.",
    "plan": "Create a file named plan_test.txt containing hi.",
}
user(PROMPTS[scenario])

for line in p.stdout:
    line = line.rstrip("\n")
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        rec("raw", line)
        continue
    rec("out", msg)
    t = msg.get("type")
    if t == "control_request":
        req = msg["request"]
        if req.get("subtype") == "can_use_tool" and req.get("tool_name") == "AskUserQuestion":
            time.sleep(ANSWER_DELAY)
            if scenario == "question_deny":
                control_ok(msg["request_id"], {"behavior": "deny", "message": "User did not answer."})
            else:
                inp = dict(req["input"])
                answers = {q["question"]: "blue" for q in inp.get("questions", [])}
                control_ok(msg["request_id"], {"behavior": "allow", "updatedInput": {**inp, "answers": answers}})
        elif req.get("subtype") == "can_use_tool" and scenario == "plan_deny":
            control_ok(msg["request_id"], {"behavior": "deny", "message": "Read-only session: writes are not allowed."})
        elif req.get("subtype") == "can_use_tool":
            control_ok(msg["request_id"], {"behavior": "allow", "updatedInput": req["input"]})
    if scenario == "interrupt" and t == "assistant":
        blocks = msg["message"].get("content", [])
        if any(b.get("type") == "tool_use" for b in blocks):
            time.sleep(3)
            send({"type": "control_request", "request_id": "int-1", "request": {"subtype": "interrupt"}})
    if t == "result":
        results += 1
        if scenario == "multi" and results == 1:
            user("Now reply with exactly: two")
            continue
        p.stdin.close()

rc = p.wait()
rec("exit", rc)
print("exit", rc, "elapsed", round(time.monotonic() - t0, 1))
