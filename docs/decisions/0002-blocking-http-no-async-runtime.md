# 2. Blocking HTTP on a thread, no async runtime, no WebSocket yet

**Status:** accepted · 2026-11-15

**Context.** The PRD names Tokio, Axum and a WebSocket stream.

**Decision.** `tiny_http` on one thread. The request handler is a plain function over the store
(`api::Api::handle`), so it is tested without sockets. Clients that want a feed poll
`GET /api/v1/history?after=<seq>`, which is gap-free.

**Why.** Traffic is local and small (10,000 events go through the handler in well under a second in the
load test). An async runtime would add memory and build time against a 100 MB idle target for no
user-visible gain.

**Revisit when** a client needs push latency under the polling interval, or remote agents arrive.
