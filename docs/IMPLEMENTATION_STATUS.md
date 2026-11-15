# Implementation status

Mapped against the AgentDesk PRD. "Done" means implemented **and exercised on Linux**: unit tests, an
end-to-end test with real processes, and driving the real window. The Windows build is cross-compiled
and type-checked, not run.

## Architecture decisions (deviations from the PRD, on purpose)

Each has a short record in [decisions/](decisions/).

| PRD | Built | Record |
|---|---|---|
| Cargo workspace, ~9 crates | 2 crates: `acc-core` (no GUI dependency: protocol, store, API, process control, SDK, `acc` CLI) and the HUD | [0001](decisions/0001-two-crates-not-nine.md) |
| Tokio + Axum + WebSocket | `tiny_http` on a thread; the handler is a plain function; clients poll `/history?after=` | [0002](decisions/0002-blocking-http-no-async-runtime.md) |
| SQLite + SQLx + migrations | JSON-lines and JSON files with retention; `serde(default)` keeps old files loading | [0003](decisions/0003-jsonl-files-not-sqlite.md) |
| wgpu (eframe default) | OpenGL (`glow`) | [0004](decisions/0004-opengl-renderer.md) |
| `tracing`, `thiserror` | a 40-line JSON-lines logger (`acc.log`) and a hand-written `store::Error` enum | no new dependencies for what a few lines do |
| Python / Rust / Node SDK | `acc_core::sdk::Client` plus dependency-free Python and Node examples | |

## Done

**Window.** Native always-on-top panel; nine tabs (Agents, Approvals, Activity, Alerts, Resources, Workspaces,
Tasks, Launch, Settings); compact mode; size and position saved within 5 s of a change and restored
(verified by moving the window, killing the HUD and restarting); empty states on every tab, a "looking for
agents" state until the first scan, and a banner for problems (bad config, unwritable data directory, failed
signal). Opacity setting (needs a compositor; not visually verified). Tray, minimise-to-tray, global hotkey,
start hidden, start at login (Linux autostart entry verified; Windows Run key written with `reg`, not run).

**Agents.** Implicit registration on first event or explicit `POST /register` with a HUD-assigned id and
capabilities (`require_registration` makes it mandatory). Execution status and connection status are separate.
The agent list is saved and restored; after a restart anything mid-run is "lost contact" and launched
processes are marked as no longer tracked. Finished agents leave the board after `stale_hours`. At most 500 agents.

**Events.** Sixteen event types (see [protocol.md](protocol.md)), a defined result for every status × event
pair, `protocol_version`, `event_id` de-duplication, per-agent `sequence` ordering, 64 KiB body cap (`413`),
agent-id validation, field length caps.

**Watchdog.** Quiet at half the ttl (visual only), lost at the ttl (one notification), recovery on the next
event, repeated-error alert (three failures in a row), runaway-duration alert, CPU / memory thresholds for
agent processes, budget alert. All off or sensible by default and configurable.

**Notifications.** Native, with severity mapped to urgency; history (Alerts tab, persisted); no repeat for an
unchanged condition; minimum severity, per-agent mute, snooze. Critical (approval needed, failure) is never
silenced. Tray attention state on Linux.

**Approvals.** Exact action shown in full, risk level (heuristic), expiry countdown, never approved by a
timeout, bound to one request id and one action hash, a changed action supersedes the old request, several
independent requests per agent, decisions timestamped and listed (Approvals tab), multiple-choice questions,
user-defined allow rules that never cover high-risk actions. No API route can answer.

**Harness.** Claude Code hooks (`acc hook`, gate modes, PostToolUse → `file.changed` / `tool.completed`),
`acc install-hooks` (merge, idempotent, backup, uninstall), `acc ask`, `acc run -- cmd` for CLIs without hooks.

**Control.** Launch, queue with a parallel limit, pause / resume with requested-then-confirmed state, two-click
stop, pause all / resume all, restart, remove; `POST /agents/<id>/command` for HUD-launched agents only.
Managed processes are stopped when the HUD quits or receives SIGTERM / SIGINT.

**Workspaces and resources.** Workspace per agent (hooks and launches report it), git branch and uncommitted
files by polling `git status` every 15 s (no filesystem watcher), same-file conflict detection from
agent-reported paths only, "N agents active here" warning, machine CPU / RAM, per-process CPU / RAM,
agent-reported tokens / cost / latency.

**Data.** History and notification retention in days, capped files trimmed on load, atomic agent-list writes,
diagnostic export (Settings button, `acc export`, `GET /export`), structured log `acc.log` rotated at 1 MiB.

**Security.** Loopback only (a non-loopback `addr` is refused), bearer token, rate limit (`429`), no route that
starts a process or answers an approval, credentials scrubbed from history, notifications, launched-agent
output, the log and exports (heuristic).

## Performance (release build, measured 2026-11-15)

Measured on a virtual X display (Xvfb) with Mesa's **software** renderer, because that is where the tests
run. Numbers on a real GPU will differ; memory in particular.

| Metric | Target | Before (v0.2.0, wgpu) | Now |
|---|---|---|---|
| Startup to window ready and API answering | < 3 s | not measured | 0.10 s |
| Idle CPU, window visible | < 2 % | 4.0 % (on the real display) | 1.9 % |
| Idle CPU, window hidden | < 2 % | not measured | 1.7 % |
| Idle memory (RSS) | < 100 MB | 142 MB (on the real display) | 118 MB, of which 63 MB is the software GL driver (`libLLVM`, `libgallium`); 86 MB proportional (PSS) |
| 50 registered / 20 active agents, ~40 events/s | works | not measured | 14.8 % CPU, 120 MB, 1.1 ms per request |
| 10,000 events through the API handler (test) | bounded | not measured | well under 1 s, all buffers capped |

What changed: OpenGL instead of wgpu; process scans read only CPU, memory and command line and skip
per-thread data; background threads wake the UI only when something changed, only while the window is
visible, and at most ten times a second. **Idle memory on a real GPU has not been measured**, so the
100 MB target is not confirmed.

## Tests

`cargo test --workspace`: 34 tests. Core: bearer check, config parsing and loopback rule, envelope / dedup /
sequence, every status transition, approvals (independence, supersede, allow rules, questions), heartbeat
stages, alerts, preferences, cleanup and caps, redaction, persistence; API: auth, registration, hostile
requests (malformed, oversized, replay, path traversal, forged answers, client-supplied pid), rate limit,
50-agent load; processes: launch / pause / resume / stop / restart / shutdown of real children, queue, git
status; CLI: hook mapping, gate rules, hook merge. End to end (`core/tests/e2e.rs`): three independent `acc`
processes over real sockets, one approved, one denied, one lost and reconnected, then a restart from the same
data directory. HUD: hotkey parsing, icon, autostart entry.

Driven by hand on the real window (Xvfb): every tab, approve / deny / choose by click, compact mode, settings
saved, export, launch → pause → confirm → stop → restart, SIGTERM cleanup, window geometry restore, hook gate
deny, `acc run`, the Python and Node examples.

## Not done, and why

- **Windows runtime testing; Windows pause / resume; Windows icon and metadata; .msi.** No Windows machine here.
- **macOS, Wayland global hotkey.** Not supported by the tray / hotkey crates in use; untestable here.
- **CI.** The workflow is committed; the account's Actions billing lock prevents it from running.
- **WebSocket stream, SQLite and migrations, async runtime.** Deferred on purpose (records 0002, 0003).
- **Filesystem watcher.** Git polling covers repositories; a watcher would add a dependency for non-git directories.
- **Tray and hotkey behaviour tests.** Verified by hand; they need a desktop session to automate.
- **Actionable notification buttons, click-through mode, answering other apps' popups, OS secure storage,
  per-agent permission profiles.** Platform-dependent or need a hook from the other app.
- **Task dependencies, capability routing, deadlines, checkpoints restore, execution replay, MCP, framework
  adapters, remote agents, multiple machines, mobile.** Later phases (P2 / P3). A launch queue exists; nothing more.
- **AppImage.** A `.deb` script exists (`packaging/build-deb.sh`).
