#!/usr/bin/env python3
"""Minimal Agent Control Centre adapter (standard library only).

Run the HUD first, then:  python3 examples/python_agent.py
"""
import json, os, sys, time, urllib.error, urllib.request, uuid

BASE = f"http://{os.environ.get('ACC_ADDR', '127.0.0.1:7777')}/api/v1"


def token() -> str:
    if os.environ.get("ACC_DATA_DIR"):
        with open(os.path.join(os.environ["ACC_DATA_DIR"], "token")) as f:
            return f.read().strip()
    root = os.environ.get("XDG_DATA_HOME") or os.path.join(os.path.expanduser("~"), ".local", "share")
    if os.name == "nt":
        root = os.environ["APPDATA"]
    with open(os.path.join(root, "agent-control-centre", "token")) as f:
        return f.read().strip()


TOKEN = token()


def call(method, path, body=None):
    req = urllib.request.Request(
        BASE + path,
        data=None if body is None else json.dumps(body).encode(),
        method=method,
        headers={"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=5) as r:
        return json.load(r)


def emit(event, agent_id="python-demo", **fields):
    # event_id makes a retry harmless: the HUD ignores an id it has already applied
    call("POST", "/event", {"protocol_version": 1, "event_id": uuid.uuid4().hex,
                            "event": event, "agent_id": agent_id, **fields})


def ask(agent_id, detail, expires_in=120):
    """Block until the user answers in the HUD. Returns True only for an explicit approval."""
    approval_id = f"{agent_id}-{uuid.uuid4().hex[:8]}"
    emit("approval.required", agent_id, approval_id=approval_id, detail=detail, expires_in=expires_in)
    deadline = time.time() + expires_in + 2
    while time.time() < deadline:
        status = call("GET", f"/approvals/{approval_id}")["status"]
        if status != "pending":
            return status == "approved"  # "expired" and "denied" are both a no
        time.sleep(0.5)
    return False


if __name__ == "__main__":
    try:
        emit("agent.started", name="Python demo", task="Tidy the build folder", ttl=30)
        for pct in (20, 50, 80):
            time.sleep(1)
            emit("agent.progress", progress=pct, message=f"working… {pct}%")
        if ask("python-demo", "$ rm -rf build/"):
            emit("agent.completed", message="build folder removed")
        else:
            emit("agent.failed", message="not approved, nothing was deleted")
    except (urllib.error.URLError, FileNotFoundError) as e:
        sys.exit(f"cannot reach the HUD or read its token: {e}")
