# Agent protocol (version 1)

Agents talk to the HUD over HTTP on `127.0.0.1:7777` (change it with `addr` in `config.json` or `ACC_ADDR`;
only loopback addresses are accepted). Nothing leaves the machine.

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
| POST | `/api/v1/register` | Ask the HUD for an agent id. Body: `name`, `capabilities` (list), `workspace`, `pid`, all optional. Returns `{"agent_id": "a-…"}`. |
| POST | `/api/v1/event` | Send one event (body ≤ 64 KiB, else `413`). |
| GET | `/api/v1/agents` | All agents. |
| GET | `/api/v1/agents/<id>` | One agent: `status`, `execution`, `connection`, `task`, `message`, `progress`, `approval`, `pid`, `capabilities`, `workspace`, `usage`, `last_seen`. |
| POST | `/api/v1/agents/<id>/command` | `{"action": "pause" \| "resume" \| "stop"}`. Only for agents launched from the HUD (`409` otherwise). |
| GET | `/api/v1/approvals` | Open requests with the exact `detail`, `options`, `risk`, `action_hash`, `expires_in`. |
| GET | `/api/v1/approvals/<id>` | `{"status": "pending" \| "approved" \| "denied" \| "answered" \| "expired", "choice": …, "action_hash": …}` |
| GET | `/api/v1/history?after=<seq>&agent=<id>&limit=<n>` | Activity entries with `seq` greater than `after`, oldest first. Poll with the last `seq` you saw for a gap-free feed. |
| GET | `/api/v1/notifications` | Notification history, including ones your preferences silenced (`shown: false`). |
| GET | `/api/v1/export` | Diagnostic report: agents, approvals, history, notifications. Credentials redacted. |

Errors are JSON: `{"error": "..."}` with `400` (bad event), `401` (token), `404`, `409` (not controllable),
`413` (body too large), `429` (more than `rate_limit` requests in one second, default 300).

There is no route that answers an approval and none that starts a process. Only the person at the HUD can do either.

`status` is `lost` when the HUD has not heard from the agent for a whole `ttl`; otherwise it equals `execution`.
`execution` (running, waiting, approval, paused, done, failed) and `connection` (connected, quiet, lost) are
reported separately, so a lost agent still shows what it was doing.

## Identity

Two ways to get an agent id:

- **Registered (recommended):** `POST /register`; the HUD picks the id. With `require_registration: true` in
  `config.json` this is the only way in.
- **Implicit:** send any event with your own `agent_id` (1–64 characters of `A-Z a-z 0-9 . _ : -`). The first
  event creates the agent. This is what hooks use, because a hook has no state between calls.

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
  "sequence": 12,
  "workspace": "/home/me/project",
  "notify": false
}
```

| Field | Required | Notes |
|-------|----------|-------|
| `event` | yes | One of the types below. |
| `agent_id` | yes | Your stable id. The first event creates the agent. |
| `protocol_version` | no | If present it must be `1`; anything else is rejected with `400`. |
| `event_id` | no | Up to 128 characters. A repeated id is ignored, so retries are safe. The HUD remembers the last 1024. |
| `sequence` | no | Per-agent counter starting at 1. An event whose `sequence` is not greater than the last one applied is ignored, so late arrivals cannot overwrite newer state. |
| `workspace` | no | Directory the agent works in. Groups agents on the Workspaces tab and enables git status. |
| `capabilities` | no | List of short labels shown on the card. |
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
| `agent.heartbeat` | Keeps the agent alive. Brings a lost agent back to connected. |
| `agent.paused` / `agent.resumed` / `agent.stopped` | The agent reports it was paused, resumed or stopped (stopped counts as failed). |
| `approval.required` | Creates an approval or a question, see below. |
| `tool.started` / `tool.completed` / `tool.failed` | A tool call, with `tool` and optional `message`. Three failures in a row raise one "keeps failing" alert. |
| `file.changed` | The agent changed `path` (absolute, or relative to `workspace`). Two agents reporting the same file within 10 minutes raise a conflict alert. |
| `resource.updated` | Adds `tokens_in`, `tokens_out`, `cost_usd` to the agent's totals; `latency_ms` replaces the last value. Not written to history. |
| `checkpoint.created` | Recorded in history with `message`. |

State rules (`next_status` in `core/src/store.rs`, tested for every status and event): heartbeat,
`resource.updated` and `checkpoint.created` never change what the agent is doing; a pause holds until
`agent.resumed`, `agent.started` or a terminal event; an agent with an open approval stays in
"needs approval" until it is answered; `agent.completed` and `agent.failed` always apply.

## Approvals

```json
{"event": "approval.required", "agent_id": "my-agent",
 "approval_id": "my-agent-17b2", "detail": "$ rm -rf build/", "expires_in": 120}
```

1. Use a **new** `approval_id` for every request. Re-sending an open id with the same `detail` is ignored.
   Re-sending it with a different `detail` replaces the request: the old one is recorded as superseded and
   can no longer be answered.
2. The HUD shows `detail` exactly as sent, with a risk level and a countdown (default 300 s, range 5–86400).
3. Poll `GET /api/v1/approvals/<approval_id>` until it is no longer `pending`. Check that `action_hash`
   is the hash of what you sent (FNV-1a 64 of `detail`, hex) before acting on the answer.
4. `approved` or `denied` is the user's decision. `expired` means nobody answered: treat it as **not approved**.
   A timeout never approves.

`acc ask` does steps 1–4 for you: exit code 0 approved, 1 denied, 2 unanswered.

**Questions.** Add `"options": ["postgres", "sqlite"]` and the HUD shows one button per option instead of
Approve / Deny. The status becomes `answered` and `choice` holds the option. `acc ask … --options "postgres|sqlite"`
prints the choice.

**Allow rules.** The user can tell the HUD to stop asking for an action ("Always allow this", or a `*`
pattern in Settings). A matching request comes back `approved` at once and is written to history as
`approval.auto`. Rules never apply to actions the HUD rates high risk (`rm -rf`, `sudo`, `--force`, pipes
into a shell, and similar): those are always asked.

## History and secrets

Every event except heartbeats and `resource.updated` is written to the activity history (kept for `retention_days`, default 30). Before it is stored, text is scrubbed
of obvious credentials (`password=…`, `--token …`, `Bearer …`, `sk-…`/`ghp_…` style keys, `user:pass@` in
URLs). The same scrubbing is applied to notifications, the output of launched agents, the log file and exports. This is a heuristic. The approval card itself still shows the exact action, because that is what
the user is approving.

## Clients

| Language | Where | Notes |
|----------|-------|-------|
| Shell | `acc emit`, `acc ask`, `acc register`, `acc run -- cmd`, `acc status`, `acc export` | `acc run` wraps any command as a monitored agent (start, heartbeat, exit code). |
| Rust | `acc_core::sdk::Client` (`core/examples/rust_agent.rs`) | `register`, `emit`, `heartbeat`, `approve`, `choose`. |
| Python | `examples/python_agent.py` | Standard library only. |
| Node 18+ | `examples/node_agent.mjs` | No dependencies. |
| Claude Code | `acc install-hooks` | Merges the hook entries into `~/.claude/settings.json` (backup written first). |

All of them read `ACC_ADDR` and `ACC_DATA_DIR` if set, which is how a second HUD instance is addressed.
