# Implementation status

Mapped against the AgentDesk PRD. "Done" means implemented **and exercised on Linux** (unit tests plus
driving the real app). The Windows build is cross-compiled and not hand-tested.

## Architecture decisions (deviations from the PRD, on purpose)

| PRD | Built | Why |
|---|---|---|
| Cargo workspace, ~9 crates | one package: lib (`store`, `procs`, `cli`) + `agent-control-centre` (UI) + `acc` (CLI) | The PRD says to pick the least complex reliable solution and not to create empty crates. Split when a module needs its own consumer. |
| Tokio + Axum + WebSocket | `tiny_http` on a thread; clients poll | Local, low volume; avoids an async runtime. WebSocket push is backlog. |
| SQLite + SQLx | append-only `history.jsonl` in the data dir (`$XDG_DATA_HOME` / `%APPDATA%`), last 2000 entries reloaded | Enough for history; SQLite is backlog once queries/retention need it. |
| egui/eframe, no JS | egui/eframe | As specified. |
| Python SDK / Rust SDK | `acc` CLI (`emit`, `ask`, `hook`) + plain HTTP | Any language can call it. A typed SDK crate is backlog. |

## Done

- Native always-on-top floating window; compact status counts; live task text; window size/position persisted (enabled, not hand-verified).
- Native notifications for approval, finished, failed, lost, attention (open approvals are not re-notified).
- **Approval centre**: exact action shown in full, expiry, never auto-approved on timeout, duplicate request is ignored, an answered approval cannot be replayed or flipped, several open approvals per agent are independent. Resolved only by the HUD user.
- **Harness integrations**: Claude Code `PreToolUse` gate (`acc hook --gate`), generic `acc ask` (exit codes 0/1/2).
- Per-agent activity history (tool calls, approvals asked/granted/denied/expired), persisted; global feed with agent filter.
- Event API: `agent.started/progress/waiting/completed/failed/heartbeat`, `approval.required`; heartbeat-lost detection with per-agent `ttl`; paused state sticks until resumed.
- Launch managed agents with streamed output; Pause / Resume / Stop (Linux: process-group signals; Windows: Stop only).
- Auto-detect running agent CLIs with CPU/memory.
- **System tray** (Linux: StatusNotifierItem via `ksni`, no GTK; Windows: `tray-icon`), **minimise-to-tray** (close hides, only when a tray really exists), **global show/hide hotkey** (default Ctrl+Alt+A, `ACC_HOTKEY`), tray menu Show/Hide / Pin / Quit, tray "needs attention" state with tooltip on Linux. Background work (heartbeat/approval expiry, managed-process exit) runs on its own thread so it continues while the window is hidden.

Tests: `cargo test` (12): state machine, approvals, history, heartbeat, launch/pause/resume/stop of a real child, process detection, hook mapping, gate rules, hotkey parsing.

Tray/hotkey were verified on Linux (X11 + a real Cinnamon StatusNotifier watcher): hotkey toggle, tray click, close-to-tray, menu Show/Hide and Quit, attention state. **Windows tray/hotkey compile but have not been run.** Wayland has no global hotkey support in the crate.

## Backlog (not implemented, no UI pretends otherwise)

- macOS tray/hotkey; Windows tray icon/tooltip reflecting pending approvals; start-hidden option.
- WebSocket stream, API auth token, rate limiting, versioned `/api/v1`, `event_id`/`sequence` dedup.
- SQLite, retention policies, export, notification history/preferences, snooze.
- Workspaces, filesystem watcher, git status, cross-agent conflict detection.
- Token / cost / latency tracking and budgets (needs agents to report usage).
- Task inbox, queue, dependencies, checkpoints/recovery, replay beyond the history list.
- MCP integration, remote agents, multiple machines, mobile.
- Answering popups that other apps draw themselves (needs a hook/API from that app).
