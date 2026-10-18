"""Record a Codex app-server session for driver fixtures.

Usage: python3 -I record_codex.py <scenario> <out.jsonl> [thread_id_for_resume]
Model: $RECORD_MODEL (default: the user's Codex default). Run it in a scratch
directory, then pass the result through sanitize.py.
"""
import json
import os
import subprocess
import sys
import threading
import time

scenario, out_path, *rest = sys.argv[1:]
t0 = time.monotonic()
log = open(out_path, "w")
lock = threading.Lock()
MODEL = os.environ.get("RECORD_MODEL")


def rec(direction, obj):
    with lock:
        log.write(json.dumps({"t": round(time.monotonic() - t0, 3), "dir": direction, "msg": obj}) + "\n")
        log.flush()


args = ["codex", "app-server"]
if scenario == "question":
    args += ["--enable", "default_mode_request_user_input"]
p = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
threading.Thread(target=lambda: [rec("stderr", l.rstrip("\n")) for l in p.stderr], daemon=True).start()

next_id = 0
pending = {}


def send(obj):
    rec("in", obj)
    p.stdin.write(json.dumps(obj) + "\n")
    p.stdin.flush()


def request(method, params):
    global next_id
    next_id += 1
    pending[next_id] = method
    send({"id": next_id, "method": method, "params": params})
    return next_id


def respond(req_id, result):
    send({"id": req_id, "result": result})


PROMPTS = {
    "basic": "Reply with exactly: pong",
    "tool": "Run the shell command `ls` and reply with the number of files.",
    "question": "Use the request_user_input tool to ask me which color I prefer, with options red and blue. Then reply with only the chosen color.",
    "approval": "Run exactly this shell command: `rm -f nonexistent_file.txt`, then reply done.",
    "interrupt": "Run the shell command `sleep 60 && echo done` and reply with its output.",
    "resume": "What word did you reply with in the previous turn? Reply with only that word.",
    "readonly": "Create a file named readonly_test.txt containing hi, using a shell command.",
}

if scenario == "approval":
    thread_opts = {"approvalPolicy": "untrusted", "sandbox": "workspace-write"}
elif scenario == "readonly":
    thread_opts = {"approvalPolicy": "never", "sandbox": "read-only"}
else:
    thread_opts = {"approvalPolicy": "never", "sandbox": "danger-full-access"}

request("initialize", {"clientInfo": {"name": "wardex-recorder", "version": "0.0.1"}, "capabilities": {"experimentalApi": True}})

thread_id = None
turn_id = None
interrupted = False
for line in p.stdout:
    line = line.rstrip("\n")
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        rec("raw", line)
        continue
    rec("out", msg)
    method = msg.get("method")
    if "id" in msg and method is None:  # response to our request
        what = pending.pop(msg["id"], None)
        if what == "initialize":
            send({"method": "initialized"})
            if scenario == "basic":
                request("model/list", {})
            if scenario == "resume":
                request("thread/resume", {"threadId": rest[0], "cwd": os.getcwd(), **thread_opts})
            else:
                params = {"cwd": os.getcwd(), **thread_opts}
                if MODEL:
                    params["model"] = MODEL
                request("thread/start", params)
        elif what in ("thread/start", "thread/resume"):
            thread_id = msg["result"]["thread"]["id"]
            params = {"threadId": thread_id, "input": [{"type": "text", "text": PROMPTS[scenario]}], "effort": "low"}
            request("turn/start", params)
        elif what == "turn/start":
            turn_id = msg["result"]["turn"]["id"]
        continue
    if "id" in msg and method:  # server -> client request
        if method == "item/tool/requestUserInput":
            time.sleep(15)
            answers = {q["id"]: {"answers": ["blue"]} for q in msg["params"]["questions"]}
            respond(msg["id"], {"answers": answers})
        elif method.endswith("requestApproval"):
            time.sleep(2)
            respond(msg["id"], {"decision": "decline"})
        else:
            send({"id": msg["id"], "error": {"code": -32601, "message": "not supported by spike"}})
        continue
    if scenario == "interrupt" and method == "item/started" and msg["params"]["item"].get("type") == "commandExecution" and not interrupted:
        interrupted = True
        time.sleep(3)
        request("turn/interrupt", {"threadId": thread_id, "turnId": turn_id})
    if method == "turn/completed":
        print("thread", thread_id)
        break

p.stdin.close()
try:
    rc = p.wait(timeout=10)
except subprocess.TimeoutExpired:
    p.kill()
    rc = "killed"
rec("exit", rc)
print("exit", rc, "elapsed", round(time.monotonic() - t0, 1))
