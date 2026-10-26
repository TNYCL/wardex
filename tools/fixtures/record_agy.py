"""Record an Antigravity CLI (`agy`) stream-json session for driver fixtures.

Usage: python3 -I record_agy.py <scenario> <out.jsonl> [conversation_id]
Env: AGY_BIN (default: agy), RECORD_MODEL, RECORD_EFFORT, RECORD_ARGS (extra
arguments, space separated). Set AGY_CLI_DISABLE_AUTO_UPDATE=1 so the
recording does not update the installed CLI. Run it in a scratch directory,
then pass the result through sanitize.py.

Scenarios: basic, tool, approval, interrupt, resume, readonly, question.
Stderr lines are recorded with `dir: "stderr"`.
"""
import json
import os
import queue
import signal
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


args = [os.environ.get("AGY_BIN", "agy"), "--input-format", "stream-json", "--output-format", "stream-json"]
if os.environ.get("RECORD_MODEL"):
    args += ["--model", os.environ["RECORD_MODEL"]]
if os.environ.get("RECORD_EFFORT"):
    args += ["--effort", os.environ["RECORD_EFFORT"]]
if scenario == "resume":
    args += ["--conversation", rest[0]]
if scenario == "readonly":
    args += ["--mode", "plan"]
args += os.environ.get("RECORD_ARGS", "").split()

PROMPTS = {
    "basic": ["Reply with exactly: pong", "Reply with exactly: ping"],
    "tool": ["Run the shell command `ls -a` and reply with the number of entries."],
    "approval": ["Run exactly this shell command: `echo hi > wardex_approval.txt`, then reply done."],
    "interrupt": ["Run the shell command `sleep 60 && echo done` and reply with its output."],
    "resume": ["What word did you reply with in the previous turn? Reply with only that word."],
    "readonly": ["Create a file named readonly_test.txt containing hi."],
    "question": ["Before doing anything else, use your tool for asking the user a question to ask me "
                 "which color I prefer, with the options red and blue. Then reply with only the chosen color."],
}
prompts = list(PROMPTS[scenario])

p = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
lines = queue.Queue()
threading.Thread(target=lambda: [lines.put(l) for l in p.stdout] + [lines.put(None)], daemon=True).start()
threading.Thread(target=lambda: [rec("stderr", l.rstrip("\n")) for l in p.stderr], daemon=True).start()


def send_prompt():
    obj = {"event": "user", "message": {"content": prompts.pop(0)}}
    rec("in", obj)
    p.stdin.write(json.dumps(obj) + "\n")
    p.stdin.flush()


send_prompt()
interrupt_at = None
deadline = time.monotonic() + 600
while time.monotonic() < deadline:
    if interrupt_at and time.monotonic() >= interrupt_at:
        interrupt_at = None
        rec("signal", "SIGINT")
        p.send_signal(signal.SIGINT)
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
    event = msg.get("event")
    if scenario == "interrupt" and interrupt_at is None and event == "step_update":
        info = msg.get("step_update", {})
        if info.get("step_type") not in (None, "user_input", "agent_response", "checkpoint"):
            interrupt_at = time.monotonic() + 3
    if event == "result":
        if prompts:
            send_prompt()
        elif not p.stdin.closed:
            p.stdin.close()
if not p.stdin.closed:
    p.stdin.close()
try:
    code = p.wait(timeout=60)
except subprocess.TimeoutExpired:
    p.kill()
    code = p.wait()
rec("exit", code)
print(f"{scenario}: exit {code}")
