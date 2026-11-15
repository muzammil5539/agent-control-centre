# 3. JSON-lines files, not SQLite

**Status:** accepted · 2026-11-15

**Context.** The PRD names SQLite with SQLx and migrations.

**Decision.** `history.jsonl` and `notifications.jsonl` (append-only, capped, trimmed on load, retention in
days), `agents.json` (rewritten atomically when it changes), `config.json`, `window.ron`. Every struct uses
`#[serde(default)]` for new fields, which is the migration story: old files keep loading.

**Why.** The only queries are "last N" and "since cursor". Files are greppable, need no migration tool,
and add no dependency.

**Revisit when** history needs indexed search across more than a few thousand entries, or tasks and
checkpoints need relations.
