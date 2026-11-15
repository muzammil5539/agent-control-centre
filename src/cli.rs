//! `acc emit` and `acc hook`: tiny HTTP clients for the HUD's event API.
use crate::store::ADDR;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

/// POST one event. Fails silently when the HUD isn't running so hooks never block an agent.
pub fn post(body: &Value) {
    let Some(addr) = ADDR.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
        return;
    };
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) else {
        return;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let body = body.to_string();
    let _ = write!(
        s,
        "POST /event HTTP/1.1\r\nHost: {ADDR}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = s.read_to_end(&mut Vec::new());
}

fn clip(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > n {
        one.chars().take(n - 1).collect::<String>() + "…"
    } else {
        one
    }
}

/// Map a Claude Code hook payload (stdin JSON) to an HUD event.
pub fn hook_to_event(v: &Value) -> Option<Value> {
    let get = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
    let session = get("session_id");
    let project = get("cwd").rsplit(['/', '\\']).next().unwrap_or("");
    let mut e = json!({
        "agent_id": format!("claude-{}", session.chars().take(8).collect::<String>()),
        "name": format!("Claude Code · {}", if project.is_empty() { "session" } else { project }),
        "ttl": 900, // long tool runs are silent; don't flag them lost
    });
    let (kind, task, msg, progress): (&str, Option<String>, Option<String>, Option<u32>) =
        match get("hook_event_name") {
            "SessionStart" => (
                "agent.started",
                Some("Session started".into()),
                None,
                Some(0),
            ),
            "UserPromptSubmit" => (
                "agent.started",
                Some(clip(get("prompt"), 90)),
                Some(String::new()),
                Some(0),
            ),
            "PreToolUse" => {
                let input = v.get("tool_input");
                let detail = ["command", "file_path", "pattern", "url", "description"]
                    .iter()
                    .find_map(|k| input.and_then(|i| i.get(*k)).and_then(Value::as_str))
                    .unwrap_or("");
                (
                    "agent.progress",
                    Some(clip(&format!("{} {}", get("tool_name"), detail), 90)),
                    None,
                    None,
                )
            }
            "Notification" => {
                let m = get("message");
                let idle = get("notification_type") == "idle_prompt"
                    || m.to_lowercase().contains("waiting for your input");
                (
                    if idle {
                        "agent.waiting"
                    } else {
                        "approval.required"
                    },
                    None,
                    Some(clip(m, 120)),
                    None,
                )
            }
            "Stop" => (
                "agent.completed",
                None,
                Some("Finished — ready for your next prompt".into()),
                None,
            ),
            "SessionEnd" => ("agent.completed", None, Some("Session ended".into()), None),
            _ => return None,
        };
    e["event"] = kind.into();
    if let Some(t) = task {
        e["task"] = t.into();
    }
    if let Some(m) = msg {
        e["message"] = m.into();
    }
    if let Some(p) = progress {
        e["progress"] = p.into();
    }
    Some(e)
}

pub fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("hook") => {
            let mut input = String::new();
            let _ = std::io::stdin().read_to_string(&mut input);
            if let Some(e) = serde_json::from_str(&input)
                .ok()
                .and_then(|v: Value| hook_to_event(&v))
            {
                post(&e);
            }
        }
        Some("emit") if args.len() >= 3 => {
            // acc emit <event> <agent_id> [--name N] [--task T] [--message M] [--progress P] [--pid PID] [--ttl S]
            let mut e = json!({"event": args[1], "agent_id": args[2]});
            for kv in args[3..].chunks(2) {
                let [k, val] = kv else { continue };
                let k = k.trim_start_matches("--");
                e[k] = if matches!(k, "progress" | "pid" | "ttl") {
                    val.parse::<u32>().map(Value::from).unwrap_or(Value::Null)
                } else {
                    val.clone().into()
                };
            }
            post(&e);
        }
        _ => eprintln!(
            "usage: acc emit <event> <agent_id> [--name N] [--task T] [--message M] [--progress P] [--pid PID] [--ttl S]\n       acc hook   (reads a Claude Code hook payload on stdin)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_hooks_map_to_events() {
        let p = |s: &str| hook_to_event(&serde_json::from_str(s).unwrap());
        let e = p(r#"{"hook_event_name":"UserPromptSubmit","session_id":"abcdef123456","cwd":"/home/me/proj","prompt":"fix the   bug"}"#).unwrap();
        assert_eq!(
            (
                e["event"].as_str(),
                e["task"].as_str(),
                e["agent_id"].as_str()
            ),
            (
                Some("agent.started"),
                Some("fix the bug"),
                Some("claude-abcdef12")
            )
        );
        assert_eq!(e["name"], "Claude Code · proj");
        let e = p(r#"{"hook_event_name":"PreToolUse","session_id":"s","tool_name":"Bash","tool_input":{"command":"cargo test"}}"#).unwrap();
        assert_eq!(e["task"], "Bash cargo test");
        let e = p(r#"{"hook_event_name":"Notification","session_id":"s","message":"Claude needs your permission to use Bash"}"#).unwrap();
        assert_eq!(e["event"], "approval.required");
        let e = p(r#"{"hook_event_name":"Notification","session_id":"s","message":"Claude is waiting for your input"}"#).unwrap();
        assert_eq!(e["event"], "agent.waiting");
        assert!(p(r#"{"hook_event_name":"PostToolUse"}"#).is_none());
    }
}
