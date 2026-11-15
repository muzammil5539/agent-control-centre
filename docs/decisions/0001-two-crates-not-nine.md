# 1. Two crates, not nine

**Status:** accepted · 2026-11-15

**Context.** The PRD sketches a workspace of about nine crates. The code base is about 4,000 lines.

**Decision.** Two crates: `acc-core` (protocol, store, local API, process control, client SDK, the `acc`
CLI; no GUI dependency) and `agent-control-centre` (the window, tray and hotkey).

**Why.** The one boundary that pays for itself is "needs a display" versus "does not": the core builds
and tests in seconds without eframe, and agents written in Rust can depend on it. More crates would be
empty shells today.

**Revisit when** a module gains a second consumer with different dependencies (for example a remote relay).
