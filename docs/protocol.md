# Agent protocol (version 1)

Agents talk to the HUD over HTTP on `127.0.0.1:7777`. Nothing leaves the machine.

## Authentication

The HUD creates a random token on first start and stores it, owner-readable only, at:

| OS | Path |
|----|------|
| Linux | `$XDG_DATA_HOME/agent-control-centre/token` (default `~/.local/share/agent-control-centre/token`) |
| Windows | `%APPDATA%\agent-control-centre\token` |

`acc token` prints the path. Send it on every request:

```
Authorization: Bearer <contents of the token file>
```

Without it every route except `GET /api/v1/health` answers `401`. This keeps other local users and
web pages (which can reach `127.0.0.1`) from posting events or reading agent state.
The token only proves the caller can read your files. It does **not** let a client answer an approval:
only the person at the HUD can do that.

## Routes

All routes live under `/api/v1`. The bare form (`/event`, `/agents`, …) still works as an alias.

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/api/v1/health` | Liveness, app version and `protocol_version`. No token needed. |
| POST | `/api/v1/event` | Send one event (body ≤ 64 KiB). |
| GET | `/api/v1/agents` | All agents: `agent_id`, `status`, `task`, `progress`, `approval`. |
| GET | `/api/v1/agents/<id>` | One agent: `status`, `approval`. |
| GET | `/api/v1/approvals` | Open approvals with the exact `detail` and `expires_in`. |
| GET | `/api/v1/approvals/<id>` | `{"status": "pending" \| "approved" \| "denied" \| "expired"}` |

Errors are JSON: `{"error": "..."}` with `400` (bad event), `401` (token), `404`.

## Event envelope

```json
{
  "protocol_version": 1,
  "event_id": "7f3c…",
  "event": "agent.progress",
  "agent_id": "my-agent",
  "name": "My Agent",
  "task": "Refactor parser",
  "message": "step 3 of 8",
  "progress": 40,
  "pid": 12345,
  "ttl": 30,
  "notify": false
}
```

| Field | Required | Notes |
|-------|----------|-------|
| `event` | yes | One of the types below. |
| `agent_id` | yes | Your stable id. The first event creates the agent. |
| `protocol_version` | no | If present it must be `1`; anything else is rejected with `400`. |
| `event_id` | no | Up to 128 characters. A repeated id is ignored, so retries are safe. The HUD remembers the last 1024. |
| `name`, `task`, `message` | no | Shown on the agent card. Kept from earlier events when omitted. |
| `progress` | no | 0–100. |
| `pid` | no | Lets the HUD show CPU and memory and offer Pause/Stop. |
| `ttl` | no | Seconds of silence before the agent is marked lost. Default 30, range 5–86400. |
| `notify` | no | `true` forces a native notification for this event. |

## Event types

| Type | Effect |
|------|--------|
| `agent.started` | Agent appears or restarts, status running. |
| `agent.progress` | Status running; updates task, message, progress. |
| `agent.waiting` | Status waiting (informational: it does not create an approval). |
| `agent.completed` | Status done, progress 100, notification. |
| `agent.failed` | Status failed, notification. |
| `agent.heartbeat` | Keeps the agent alive. Brings a lost agent back to running. |
| `approval.required` | Creates an approval, see below. |

Paused stays paused until the user resumes, and an agent with an open approval stays in
"needs approval" until it is answered.

## Approvals

```json
{"event": "approval.required", "agent_id": "my-agent",
 "approval_id": "my-agent-17b2", "detail": "$ rm -rf build/", "expires_in": 120}
```

1. Use a **new** `approval_id` for every request. Re-sending an id that is still open is ignored.
2. The HUD shows `detail` exactly as sent, with a countdown (default 300 s, range 5–86400).
3. Poll `GET /api/v1/approvals/<approval_id>` until it is no longer `pending`.
4. `approved` or `denied` is the user's decision. `expired` means nobody answered: treat it as **not approved**.
   The HUD never approves on its own.

`acc ask` does steps 1–4 for you: exit code 0 approved, 1 denied, 2 unanswered.

## History and secrets

Every event except heartbeats is written to the activity history. Before it is stored, text is scrubbed
of obvious credentials (`password=…`, `--token …`, `Bearer …`, `sk-…`/`ghp_…` style keys, `user:pass@` in
URLs). This is a heuristic. The approval card itself still shows the exact action, because that is what
the user is approving.

## Examples

`examples/python_agent.py` is a complete adapter using only the Python standard library.
