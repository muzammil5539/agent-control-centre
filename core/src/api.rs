//! The local HTTP API as a function over the store, so it is tested without sockets or a window.
use crate::procs::{self, Sig};
use crate::store::{Event, PROTOCOL_VERSION, Source, Store, now_secs};
use crate::{auth, log};
use serde_json::{Value, json};
use std::{
    io::Read,
    sync::{Arc, Mutex},
};

pub const MAX_BODY: usize = 65536; // a rogue local process can't make us buffer forever

pub struct Api {
    pub token: String,
    window: (u64, u32), // (second, requests seen in it)
}

fn query(url: &str) -> (&str, Vec<(&str, &str)>) {
    let (path, q) = url.split_once('?').unwrap_or((url, ""));
    (
        path,
        q.split('&').filter_map(|kv| kv.split_once('=')).collect(),
    )
}

impl Api {
    pub fn new(token: String) -> Self {
        Api {
            token,
            window: (0, 0),
        }
    }

    /// One request. `/api/v1/x` is the canonical route; the bare `/x` form is an alias.
    pub fn handle(
        &mut self,
        s: &mut Store,
        method: &str,
        url: &str,
        bearer: Option<&str>,
        body: &str,
    ) -> (u16, Value) {
        let now = now_secs();
        if self.window.0 != now {
            self.window = (now, 0);
        }
        self.window.1 += 1;
        if self.window.1 > s.cfg.rate_limit.max(1) {
            return (429, json!({"error": "rate limit exceeded; slow down"}));
        }
        let (path, q) = query(url.strip_prefix("/api/v1").unwrap_or(url));
        let arg = |k: &str| q.iter().find(|(key, _)| *key == k).map(|(_, v)| *v);
        if (method, path) == ("GET", "/health") {
            return (
                200,
                json!({"ok": true, "version": env!("CARGO_PKG_VERSION"), "protocol_version": PROTOCOL_VERSION}),
            );
        }
        if !auth::check(bearer, &self.token) {
            log::warn(
                "request refused: missing or wrong token",
                json!({"path": path}),
            );
            return (
                401,
                json!({"error": "missing or wrong token; see `acc token` for the file to read"}),
            );
        }
        if body.len() > MAX_BODY {
            return (413, json!({"error": "body too large (64 KiB limit)"}));
        }
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (method, parts.as_slice()) {
            ("POST", ["event"]) => match serde_json::from_str::<Event>(body) {
                Err(e) => (400, json!({"error": e.to_string()})),
                Ok(e) => match s.apply(e) {
                    Ok(()) => (200, json!({"ok": true})),
                    Err(e) => {
                        log::warn("event refused", json!({"reason": e.to_string()}));
                        (400, json!({"error": e.to_string()}))
                    }
                },
            },
            ("POST", ["register"]) => {
                let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                let caps = v["capabilities"].as_array().map_or(vec![], |a| {
                    a.iter().filter_map(|c| c.as_str().map(String::from)).collect()
                });
                match s.register(
                    v["name"].as_str().unwrap_or(""),
                    caps,
                    v["workspace"].as_str().unwrap_or(""),
                    v["pid"].as_u64().map(|p| p as u32),
                ) {
                    Ok(id) => (200, json!({"agent_id": id, "protocol_version": PROTOCOL_VERSION})),
                    Err(e) => (429, json!({"error": e.to_string()})),
                }
            }
            ("GET", ["agents"]) => (200, s.agents.iter().map(|(id, a)| Store::agent_json(id, a)).collect()),
            ("GET", ["agents", id]) => match s.agents.get(*id) {
                Some(a) => (200, Store::agent_json(id, a)),
                None => (404, json!({"error": "unknown agent"})),
            },
            // Control is limited to processes the user launched from the HUD: a client must not be able
            // to point the HUD at an arbitrary pid. The API can never start a process.
            ("POST", ["agents", id, "command"]) => {
                let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                let sig = match v["action"].as_str() {
                    Some("pause") => Sig::Pause,
                    Some("resume") => Sig::Resume,
                    Some("stop") => Sig::Stop,
                    _ => return (400, json!({"error": "action must be pause, resume or stop"})),
                };
                match s.agents.get(*id).map(|a| a.source) {
                    None => (404, json!({"error": "unknown agent"})),
                    Some(Source::Api) => (409, json!({"error": crate::store::Error::NotControllable.to_string()})),
                    Some(Source::Managed) => match procs::control(s, id, sig) {
                        Ok(()) => (200, json!({"ok": true})),
                        Err(e) => (409, json!({"error": e.to_string()})),
                    },
                }
            }
            ("GET", ["approvals"]) => (
                200,
                s.pending_approvals()
                    .map(|a| json!({"approval_id": a.id, "agent_id": a.agent_id, "detail": a.detail, "options": a.options,
                        "risk": a.risk.label(), "action_hash": a.hash, "expires_in": a.remaining().as_secs()}))
                    .collect(),
            ),
            // There is deliberately no route that answers an approval: only the person at the HUD can.
            ("GET", ["approvals", id]) => match s.approvals.iter().find(|a| a.id == *id) {
                Some(a) => (200, json!({"status": a.status(), "choice": a.choice, "action_hash": a.hash})),
                None => (404, json!({"error": "unknown approval"})),
            },
            // Poll with ?after=<last seq seen> for a gap-free feed (the stand-in for a push stream).
            ("GET", ["history"]) => {
                let after = arg("after").and_then(|v| v.parse().ok()).unwrap_or(0u64);
                let limit = arg("limit").and_then(|v| v.parse().ok()).unwrap_or(200usize).min(1000);
                let agent = arg("agent");
                let list: Vec<_> = s
                    .history
                    .iter()
                    .filter(|e| e.seq > after && agent.is_none_or(|a| a == e.agent_id))
                    .take(limit)
                    .collect();
                (200, json!(list))
            }
            ("GET", ["notifications"]) => (200, json!(s.notes)),
            ("GET", ["export"]) => (200, s.export()),
            _ => (404, json!({"error": "not found"})),
        }
    }
}

/// Serve until the process exits. `changed` runs after every request that may have altered state.
pub fn serve(
    server: tiny_http::Server,
    store: Arc<Mutex<Store>>,
    mut api: Api,
    changed: impl Fn(),
) {
    for mut req in server.incoming_requests() {
        let method = req.method().to_string();
        let url = req.url().to_string();
        let bearer = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("Authorization"))
            .map(|h| h.value.to_string());
        let mut buf = String::new();
        // one byte past the cap, so an oversized body is seen as oversized rather than as broken JSON
        let _ = req
            .as_reader()
            .take(MAX_BODY as u64 + 1)
            .read_to_string(&mut buf);
        let (code, body) = {
            let mut s = store.lock().unwrap();
            s.sweep(1.0); // answers stay correct even if the ticker is behind
            api.handle(&mut s, &method, &url, bearer.as_deref(), &buf)
        };
        if method != "GET" {
            changed();
        }
        let h = tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap();
        let _ = req.respond(
            tiny_http::Response::from_string(body.to_string())
                .with_status_code(code)
                .with_header(h),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Status;

    const T: Option<&str> = Some("Bearer tok");
    fn api() -> (Api, Store) {
        (Api::new("tok".into()), Store::default())
    }

    #[test]
    fn auth_routes_and_registration() {
        let (mut a, mut s) = api();
        assert_eq!(a.handle(&mut s, "GET", "/api/v1/health", None, "").0, 200);
        assert_eq!(a.handle(&mut s, "GET", "/api/v1/agents", None, "").0, 401);
        assert_eq!(
            a.handle(&mut s, "GET", "/agents", Some("Bearer nope"), "")
                .0,
            401
        );
        let (code, v) = a.handle(
            &mut s,
            "POST",
            "/api/v1/register",
            T,
            r#"{"name":"Builder","capabilities":["rust"]}"#,
        );
        let id = v["agent_id"].as_str().unwrap().to_string();
        assert_eq!(code, 200);
        let ev = format!(r#"{{"event":"agent.started","agent_id":"{id}","task":"compile"}}"#);
        assert_eq!(a.handle(&mut s, "POST", "/api/v1/event", T, &ev).0, 200);
        let (_, v) = a.handle(&mut s, "GET", &format!("/api/v1/agents/{id}"), T, "");
        assert_eq!(
            (
                v["execution"].as_str(),
                v["connection"].as_str(),
                v["capabilities"][0].as_str()
            ),
            (Some("running"), Some("connected"), Some("rust"))
        );
        let (_, v) = a.handle(&mut s, "GET", "/api/v1/history?after=1&limit=5", T, "");
        assert_eq!(v.as_array().unwrap().len(), 1); // cursor: only what came after seq 1
        assert_eq!(
            a.handle(&mut s, "GET", "/api/v1/export", T, "").1["agents"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    /// SEC-07: malformed, oversized, replayed and path-traversal requests are refused.
    #[test]
    fn hostile_requests_are_refused() {
        let (mut a, mut s) = api();
        for body in [
            "",
            "{",
            "[]",
            r#"{"event":"agent.started"}"#,
            r#"{"event":"nope","agent_id":"a"}"#,
            r#"{"event":"agent.started","agent_id":"../../etc/passwd"}"#,
            r#"{"event":"agent.started","agent_id":"a","progress":"lots"}"#,
        ] {
            assert_eq!(a.handle(&mut s, "POST", "/event", T, body).0, 400, "{body}");
        }
        let big = format!(
            r#"{{"event":"agent.started","agent_id":"a","task":"{}"}}"#,
            "x".repeat(MAX_BODY)
        );
        assert_eq!(a.handle(&mut s, "POST", "/event", T, &big).0, 413);
        assert!(s.agents.is_empty()); // nothing above created an agent
        for path in [
            "/agents/../../etc/passwd",
            "/approvals/..%2f..%2fetc",
            "/../token",
            "/api/v1/../../token",
            "/approvals/x/answer",
        ] {
            assert_eq!(a.handle(&mut s, "GET", path, T, "").0, 404, "{path}");
        }
        // an answer can't be forged or replayed through the API: there is no route for it
        let ask = r#"{"event":"approval.required","agent_id":"a","approval_id":"r1","detail":"git push","event_id":"e1"}"#;
        assert_eq!(a.handle(&mut s, "POST", "/event", T, ask).0, 200);
        for (m, p) in [
            ("POST", "/approvals/r1"),
            ("POST", "/approvals/r1/answer"),
            ("PUT", "/approvals/r1"),
        ] {
            assert_eq!(a.handle(&mut s, m, p, T, r#"{"answer":true}"#).0, 404);
        }
        assert_eq!(
            a.handle(&mut s, "GET", "/approvals/r1", T, "").1["status"],
            "pending"
        );
        s.answer("r1", false);
        assert_eq!(a.handle(&mut s, "POST", "/event", T, ask).0, 200); // replay of the same event id
        assert_eq!(
            a.handle(&mut s, "GET", "/approvals/r1", T, "").1["status"],
            "denied"
        ); // still denied
        // a client can't make the HUD signal a pid it supplied
        let _ = a.handle(
            &mut s,
            "POST",
            "/event",
            T,
            r#"{"event":"agent.started","agent_id":"x","pid":1}"#,
        );
        assert_eq!(
            a.handle(
                &mut s,
                "POST",
                "/agents/x/command",
                T,
                r#"{"action":"stop"}"#
            )
            .0,
            409
        );
        assert_eq!(
            a.handle(
                &mut s,
                "POST",
                "/agents/x/command",
                T,
                r#"{"action":"launch"}"#
            )
            .0,
            400
        );
    }

    #[test]
    fn rate_limit_pushes_back() {
        let (mut a, mut s) = api();
        s.cfg.rate_limit = 50;
        let codes: Vec<u16> = (0..400)
            .map(|_| a.handle(&mut s, "GET", "/agents", T, "").0)
            .collect();
        assert!(codes.contains(&429) && codes[0] == 200);
    }

    /// PERF-04: 50 registered agents, 20 of them active, 10,000 events. Everything stays bounded.
    #[test]
    fn load_50_agents_20_active() {
        let (mut a, mut s) = api();
        s.cfg.rate_limit = u32::MAX;
        let ids: Vec<String> = (0..50)
            .map(|i| {
                a.handle(
                    &mut s,
                    "POST",
                    "/register",
                    T,
                    &format!(r#"{{"name":"agent {i}"}}"#),
                )
                .1["agent_id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        let t = std::time::Instant::now();
        for n in 0..10_000u32 {
            let id = &ids[(n % 20) as usize];
            let body = format!(
                r#"{{"event":"agent.progress","agent_id":"{id}","task":"step {n}","progress":{},"sequence":{n},"event_id":"{id}-{n}"}}"#,
                n % 100
            );
            assert_eq!(a.handle(&mut s, "POST", "/event", T, &body).0, 200);
        }
        let took = t.elapsed();
        assert_eq!(s.agents.len(), 50);
        assert_eq!(s.count(Status::Running), 20);
        assert!(s.history.len() <= 2000 && s.notes.len() <= 300); // bounded buffers
        assert!(took.as_secs() < 5, "10k events took {took:?}");
        eprintln!("10,000 events through the API handler in {took:?}");
    }
}
