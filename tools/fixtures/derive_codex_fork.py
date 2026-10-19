# Derives crates/wardex-drivers/tests/fixtures/codex/fork.jsonl from the
# recorded resume.jsonl (Codex 0.163.0). `thread/fork` was not recorded live:
# its request and response follow the 0.163.0 schema
# (`codex app-server generate-json-schema`: ThreadForkParams/Response, the
# same response shape as thread/resume plus `forkedFromId`).
#   python3 tools/fixtures/derive_codex_fork.py
import json, os

here = os.path.join(os.path.dirname(__file__), "..", "..", "crates", "wardex-drivers", "tests", "fixtures", "codex")
SOURCE = "01a15013-9936-7f63-9424-379ccce357ea"
FORK = "01a15013-9936-7f63-9424-3fa4cce357eb"


def swap(value):
    if isinstance(value, dict):
        return {k: swap(v) for k, v in value.items()}
    if isinstance(value, list):
        return [swap(v) for v in value]
    if value == SOURCE:
        return FORK
    return value


out = []
for line in open(os.path.join(here, "resume.jsonl")):
    rec = json.loads(line)
    msg = rec["msg"]
    if isinstance(msg, dict) and msg.get("method") == "deprecationNotice":
        continue  # only sent without excludeTurns
    if isinstance(msg, dict) and msg.get("method") == "thread/resume":
        params = dict(msg["params"], threadId=SOURCE, excludeTurns=True)
        rec["msg"] = {"id": msg["id"], "method": "thread/fork", "params": params}
    elif isinstance(msg, dict) and msg.get("id") == 2 and "result" in msg:
        result = swap(msg["result"])
        result["thread"]["forkedFromId"] = SOURCE
        result["thread"]["turns"] = []
        rec["msg"] = {"id": 2, "result": result}
    else:
        rec["msg"] = swap(msg)
    out.append(rec)
with open(os.path.join(here, "fork.jsonl"), "w") as f:
    for rec in out:
        f.write(json.dumps(rec) + "\n")
