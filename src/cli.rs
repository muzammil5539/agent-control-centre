//! `acc emit` and `acc hook`: tiny HTTP clients for the HUD's event API.
use crate::store::ADDR;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

/// One HTTP round trip to the HUD. None when it isn't running (so hooks never block an agent).
fn request(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    let addr = ADDR.to_socket_addrs().ok()?.next()?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "{method} {path} HTTP/1.1\r\nHost: {ADDR}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    let code = out.split_whitespace().nth(1)?.parse().ok()?;
    Some((
        code,
        out.split_once("\r\n\r\n")
            .map(|x| x.1)
            .unwrap_or("")
            .to_string(),
    ))
}

pub fn post(body: &Value) -> bool {
    request("POST", "/event", &body.to_string()).is_some_and(|(c, _)| c == 200)
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Approved,
    Denied,
    Unanswered, // expired, or the HUD isn't running: callers must treat this as NOT approved
}

/// Ask the user to approve one exact action and wait for the answer (never auto-approves).
pub fn ask(agent_id: &str, name: Option<&str>, detail: &str, timeout: u32) -> Verdict {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let id = format!("{agent_id}-{nanos:x}");
    let mut e = json!({"event": "approval.required", "agent_id": agent_id, "approval_id": id, "detail": clip(detail, 2000), "expires_in": timeout});
    if let Some(n) = name {
        e["name"] = n.into();
    }
    if !post(&e) {
        return Verdict::Unanswered;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout as u64 + 2);
    while std::time::Instant::now() < deadline {
        match request("GET", &format!("/approvals/{id}"), "") {
            Some((200, b)) if b.contains("\"approved\"") => return Verdict::Approved,
            Some((200, b)) if b.contains("\"denied\"") => return Verdict::Denied,
            Some((200, b)) if b.contains("\"expired\"") => return Verdict::Unanswered,
            None => return Verdict::Unanswered,
            _ => std::thread::sleep(Duration::from_millis(400)),
        }
    }
    Verdict::Unanswered
}

/// Which Claude Code tool calls the HUD gates: off | bash | mutating | all.
pub fn gate_applies(gate: &str, tool: &str) -> bool {
    match gate {
        "bash" => tool == "Bash",
        "mutating" => matches!(
            tool,
            "Bash" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit"
        ),
        "all" => true,
        _ => false,
    }
}

/// Full text of the action awaiting approval (what the user must see before saying yes).
pub fn describe(v: &Value) -> String {
    let tool = v.get("tool_name").and_then(Value::as_str).unwrap_or("tool");
    let input = v.get("tool_input").cloned().unwrap_or(Value::Null);
    let main = ["command", "file_path", "notebook_path", "url", "pattern"]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str));
    match main {
        Some(m) if tool == "Bash" => format!("$ {m}"),
        Some(m) => format!("{tool}: {m}"),
        None => format!("{tool}: {input}"),
    }
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
            // Claude's own permission prompt can't be answered from here (only gated tool calls can),
            // so this is an attention signal, not an approval request.
            "Notification" => ("agent.waiting", None, Some(clip(get("message"), 120)), None),
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
    if get("hook_event_name") == "Notification" {
        e["notify"] = true.into();
    }
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

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

pub fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("hook") => {
            let gate = flag(&args, "--gate")
                .or_else(|| std::env::var("ACC_GATE").ok())
                .unwrap_or_else(|| "off".into());
            let timeout = flag(&args, "--timeout")
                .or_else(|| std::env::var("ACC_GATE_TIMEOUT").ok())
                .and_then(|t| t.parse().ok())
                .unwrap_or(120);
            let mut input = String::new();
            let _ = std::io::stdin().read_to_string(&mut input);
            let Some(v) = serde_json::from_str::<Value>(&input).ok() else {
                return;
            };
            let Some(e) = hook_to_event(&v) else { return };
            post(&e);
            let tool = v.get("tool_name").and_then(Value::as_str).unwrap_or("");
            if v.get("hook_event_name").and_then(Value::as_str) == Some("PreToolUse")
                && gate_applies(&gate, tool)
            {
                let (id, name) = (
                    e["agent_id"].as_str().unwrap_or("claude"),
                    e["name"].as_str(),
                );
                let (decision, why) = match ask(id, name, &describe(&v), timeout) {
                    Verdict::Approved => ("allow", "Approved in Agent Control Centre"),
                    Verdict::Denied => ("deny", "Denied in Agent Control Centre"),
                    Verdict::Unanswered => return, // fall back to the agent's own prompt
                };
                println!(
                    "{}",
                    json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": decision, "permissionDecisionReason": why}})
                );
            }
        }
        Some("ask") if args.len() >= 2 => {
            // acc ask <agent_id> --detail "what you want to do" [--name N] [--timeout S]  → exit 0 approved, 1 denied, 2 unanswered
            let detail = flag(&args, "--detail").unwrap_or_default();
            let timeout = flag(&args, "--timeout")
                .and_then(|t| t.parse().ok())
                .unwrap_or(120);
            let code = match ask(&args[1], flag(&args, "--name").as_deref(), &detail, timeout) {
                Verdict::Approved => 0,
                Verdict::Denied => 1,
                Verdict::Unanswered => 2,
            };
            std::process::exit(code);
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
            "usage: acc emit <event> <agent_id> [--name N] [--task T] [--message M] [--progress P] [--pid PID] [--ttl S]\n       acc ask <agent_id> --detail TEXT [--name N] [--timeout S]   (exit 0 approved / 1 denied / 2 unanswered)\n       acc hook [--gate off|bash|mutating|all] [--timeout S]   (reads a Claude Code hook payload on stdin)"
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
        assert_eq!(
            (e["event"].as_str(), e["notify"].as_bool()),
            (Some("agent.waiting"), Some(true))
        ); // informational; only gated calls can be answered
        assert!(p(r#"{"hook_event_name":"PostToolUse"}"#).is_none());
    }

    #[test]
    fn gate_and_describe() {
        assert!(gate_applies("bash", "Bash") && !gate_applies("bash", "Edit"));
        assert!(gate_applies("mutating", "Write") && !gate_applies("mutating", "Read"));
        assert!(gate_applies("all", "Read") && !gate_applies("off", "Bash"));
        let v: Value =
            serde_json::from_str(r#"{"tool_name":"Bash","tool_input":{"command":"rm -rf build"}}"#)
                .unwrap();
        assert_eq!(describe(&v), "$ rm -rf build");
    }

    #[test]
    fn unanswered_when_hud_is_not_running() {
        // nothing listens on the port in unit tests (or the HUD answers): never approve by default
        if std::net::TcpStream::connect(ADDR).is_err() {
            assert_eq!(ask("t", None, "x", 5), Verdict::Unanswered);
        }
    }
}
