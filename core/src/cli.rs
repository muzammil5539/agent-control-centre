//! The `acc` command: a tiny HTTP client for the HUD's API, the coding-agent hook adapter,
//! a wrapper that turns any command into a monitored agent, and the hook installer.
use crate::config::addr;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

/// One HTTP round trip to the HUD. None when it isn't running (so hooks never block an agent).
pub fn request(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    let host = addr();
    let sock = host.to_socket_addrs().ok()?.next()?;
    let mut s = TcpStream::connect_timeout(&sock, Duration::from_millis(300)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let auth =
        crate::auth::read().map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(s, "{method} /api/v1{path} HTTP/1.1\r\nHost: {host}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).ok()?;
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
    match ask_with(agent_id, name, detail, &[], timeout) {
        Some(a) if a == "approved" => Verdict::Approved,
        Some(a) if a == "denied" => Verdict::Denied,
        _ => Verdict::Unanswered,
    }
}

/// Ask and wait. Returns "approved", "denied" or, when `options` are offered, the chosen option.
/// None = no answer in time, the HUD isn't running, or the answer was for a different action.
pub fn ask_with(
    agent_id: &str,
    name: Option<&str>,
    detail: &str,
    options: &[String],
    timeout: u32,
) -> Option<String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let id = format!("{agent_id}-{nanos:x}");
    let detail = clip(detail, 2000);
    let mut e = json!({"event": "approval.required", "agent_id": agent_id, "approval_id": id, "detail": detail, "expires_in": timeout, "options": options});
    if let Some(n) = name {
        e["name"] = n.into();
    }
    if !post(&e) {
        return None;
    }
    let hash = crate::store::action_hash(&detail);
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout as u64 + 2);
    while std::time::Instant::now() < deadline {
        let (code, body) = request("GET", &format!("/approvals/{id}"), "")?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        if code != 200 || v["action_hash"].as_str() != Some(&hash) {
            return None; // not the action we asked about: never treat it as approved
        }
        match v["status"].as_str() {
            Some("approved") => return Some("approved".into()),
            Some("denied") => return Some("denied".into()),
            Some("answered") => return v["choice"].as_str().map(String::from),
            Some("pending") => std::thread::sleep(Duration::from_millis(400)),
            _ => return None,
        }
    }
    None
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
    if !get("cwd").is_empty() {
        e["workspace"] = get("cwd").into();
    }
    // After a tool ran: file edits are reported as file.changed (reliable attribution), the rest as tool.completed.
    if get("hook_event_name") == "PostToolUse" {
        let tool = get("tool_name");
        let path = v
            .get("tool_input")
            .and_then(|i| i.get("file_path").or(i.get("notebook_path")))
            .and_then(Value::as_str);
        match path {
            Some(p) if matches!(tool, "Write" | "Edit" | "MultiEdit" | "NotebookEdit") => {
                e["event"] = "file.changed".into();
                e["path"] = p.into();
            }
            _ => {
                e["event"] = "tool.completed".into();
                e["tool"] = tool.into();
            }
        }
        return Some(e);
    }
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

const HOOK_EVENTS: [&str; 7] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    "SessionEnd",
];

/// Merge (or remove) the `acc hook` entries in a Claude Code settings file. Other hooks and
/// settings are left as they are; running it twice changes nothing.
pub fn merge_hooks(settings: &mut Value, acc: &str, gate: &str, timeout: u32, remove: bool) {
    if !settings.is_object() {
        *settings = json!({});
    }
    if !settings["hooks"].is_object() {
        settings["hooks"] = json!({});
    }
    let ours = |h: &Value| {
        h["command"]
            .as_str()
            .is_some_and(|c| c.contains("acc") && c.contains(" hook"))
    };
    for ev in HOOK_EVENTS {
        let mut groups: Vec<Value> = settings["hooks"][ev]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for g in &mut groups {
            if let Some(hs) = g["hooks"].as_array_mut() {
                hs.retain(|h| !ours(h));
            }
        }
        groups.retain(|g| g["hooks"].as_array().is_none_or(|h| !h.is_empty()));
        if !remove {
            groups.push(if ev == "PreToolUse" && gate != "off" {
                json!({"hooks": [{"type": "command", "command": format!("{acc} hook --gate {gate} --timeout {timeout}"), "timeout": timeout + 10}]})
            } else {
                json!({"hooks": [{"type": "command", "command": format!("{acc} hook")}]})
            });
        }
        if groups.is_empty() {
            if let Some(o) = settings["hooks"].as_object_mut() {
                o.remove(ev);
            }
        } else {
            settings["hooks"][ev] = groups.into();
        }
    }
}

fn install_hooks(args: &[String]) -> Result<String, String> {
    let path = match flag(args, "--settings") {
        Some(p) => std::path::PathBuf::from(p),
        None => std::path::PathBuf::from(
            std::env::var_os("HOME")
                .or(std::env::var_os("USERPROFILE"))
                .ok_or("no home directory")?,
        )
        .join(".claude/settings.json"),
    };
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
    let mut v: Value = serde_json::from_str(&text).map_err(|e| {
        format!(
            "{} is not valid JSON ({e}); not touching it",
            path.display()
        )
    })?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let acc = format!("\"{}\"", exe.display());
    let remove = args.iter().any(|a| a == "--uninstall");
    let gate = flag(args, "--gate").unwrap_or_else(|| "mutating".into());
    if !matches!(gate.as_str(), "off" | "bash" | "mutating" | "all") {
        return Err("--gate must be off, bash, mutating or all".into());
    }
    merge_hooks(
        &mut v,
        &acc,
        &gate,
        flag(args, "--timeout")
            .and_then(|t| t.parse().ok())
            .unwrap_or(120),
        remove,
    );
    if path.exists() {
        std::fs::copy(&path, path.with_extension("json.acc-backup"))
            .map_err(|e| format!("backup failed: {e}"))?;
    }
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&v).unwrap_or_default() + "\n",
    )
    .map_err(|e| e.to_string())?;
    Ok(format!(
        "{} hooks in {} (gate: {})",
        if remove { "removed" } else { "installed" },
        path.display(),
        if remove { "-" } else { &gate }
    ))
}

/// `acc run [--name N] -- cmd args…`: run any command as a monitored agent. Works for agent CLIs
/// that have no hook system: the HUD sees it start, stay alive, and finish or fail.
fn run(args: &[String]) -> i32 {
    let Some(split) = args.iter().position(|a| a == "--") else {
        eprintln!("usage: acc run [--name N] -- command [args…]");
        return 2;
    };
    let cmd = &args[split + 1..];
    let Some(prog) = cmd.first() else { return 2 };
    let name = flag(&args[..split], "--name").unwrap_or_else(|| prog.clone());
    let cwd = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let mut child = match std::process::Command::new(prog).args(&cmd[1..]).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("acc run: cannot start {prog}: {e}");
            return 127;
        }
    };
    let id = format!("run-{}", child.id());
    post(
        &json!({"event": "agent.started", "agent_id": id, "name": name, "task": clip(&cmd.join(" "), 200), "pid": child.id(), "workspace": cwd, "ttl": 30}),
    );
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code().unwrap_or(1),
            Ok(None) => {}
            Err(_) => break 1,
        }
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(500)); // heartbeat every 10 s, exit noticed within 0.5 s
            if !matches!(child.try_wait(), Ok(None)) {
                break;
            }
        }
        post(&json!({"event": "agent.heartbeat", "agent_id": id}));
    };
    if code == 0 {
        post(&json!({"event": "agent.completed", "agent_id": id, "message": "exited with code 0"}));
    } else {
        post(
            &json!({"event": "agent.failed", "agent_id": id, "message": format!("exited with code {code}")}),
        );
    }
    code
}

const USAGE: &str = "usage: acc emit <event> <agent_id> [--name N] [--task T] [--message M] [--progress P] [--pid PID] [--ttl S]
                [--workspace DIR] [--path FILE] [--tool T] [--tokens_in N] [--tokens_out N] [--cost_usd X] [--sequence N]
       acc ask <agent_id> --detail TEXT [--name N] [--timeout S] [--options \"a|b|c\"]
                (exit 0 approved / 1 denied / 2 unanswered; with --options the choice is printed)
       acc register <name> [--capabilities a,b] [--workspace DIR]   (prints the agent id the HUD assigned)
       acc run [--name N] -- command [args…]   (run any command as a monitored agent)
       acc hook [--gate off|bash|mutating|all] [--timeout S]   (reads a Claude Code hook payload on stdin)
       acc install-hooks [--gate G] [--timeout S] [--uninstall] [--settings FILE]   (Claude Code settings.json)
       acc status | acc export   (print agents / a diagnostic report as JSON)
       acc token   (print the path of the API token file)";

pub fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("hook") => {
            let cfg = crate::config::Config::load().0;
            let gate = flag(&args, "--gate")
                .or_else(|| std::env::var("ACC_GATE").ok())
                .unwrap_or(cfg.gate);
            let timeout = flag(&args, "--timeout")
                .or_else(|| std::env::var("ACC_GATE_TIMEOUT").ok())
                .and_then(|t| t.parse().ok())
                .unwrap_or(cfg.gate_timeout);
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
            let detail = flag(&args, "--detail").unwrap_or_default();
            let timeout = flag(&args, "--timeout")
                .and_then(|t| t.parse().ok())
                .unwrap_or(120);
            let options: Vec<String> = flag(&args, "--options")
                .map(|o| {
                    o.split('|')
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let code = match ask_with(
                &args[1],
                flag(&args, "--name").as_deref(),
                &detail,
                &options,
                timeout,
            )
            .as_deref()
            {
                Some("approved") => 0,
                Some("denied") => 1,
                Some(choice) => {
                    println!("{choice}");
                    0
                }
                None => 2,
            };
            std::process::exit(code);
        }
        Some("emit") if args.len() >= 3 => {
            let mut e = json!({"event": args[1], "agent_id": args[2]});
            for kv in args[3..].chunks(2) {
                let [k, val] = kv else { continue };
                let k = k.trim_start_matches("--");
                e[k] = match k {
                    "progress" | "pid" | "ttl" | "sequence" | "tokens_in" | "tokens_out"
                    | "latency_ms" | "expires_in" => {
                        val.parse::<u64>().map(Value::from).unwrap_or(Value::Null)
                    }
                    "cost_usd" => val.parse::<f64>().map(Value::from).unwrap_or(Value::Null),
                    _ => val.clone().into(),
                };
            }
            if !post(&e) {
                eprintln!(
                    "not delivered: is the HUD running? (`acc token` shows the token file it uses)"
                );
                std::process::exit(1);
            }
        }
        Some("register") if args.len() >= 2 => {
            let caps: Vec<String> = flag(&args, "--capabilities")
                .map(|c| c.split(',').map(String::from).collect())
                .unwrap_or_default();
            let body = json!({"name": args[1], "capabilities": caps, "workspace": flag(&args, "--workspace").unwrap_or_default()});
            match request("POST", "/register", &body.to_string())
                .and_then(|(c, b)| {
                    (c == 200)
                        .then(|| serde_json::from_str::<Value>(&b).ok())
                        .flatten()
                })
                .and_then(|v| v["agent_id"].as_str().map(String::from))
            {
                Some(id) => println!("{id}"),
                None => {
                    eprintln!("not registered: is the HUD running?");
                    std::process::exit(1);
                }
            }
        }
        Some("run") => std::process::exit(run(&args[1..])),
        Some("install-hooks") => match install_hooks(&args) {
            Ok(m) => println!("{m}"),
            Err(e) => {
                eprintln!("acc install-hooks: {e}");
                std::process::exit(1);
            }
        },
        Some(cmd @ ("status" | "export")) => {
            match request(
                "GET",
                if cmd == "status" {
                    "/agents"
                } else {
                    "/export"
                },
                "",
            ) {
                Some((200, b)) => println!("{}", b.trim()),
                _ => {
                    eprintln!("no answer: is the HUD running?");
                    std::process::exit(1);
                }
            }
        }
        Some("token") => match crate::auth::token_path() {
            Some(p) => println!("{}", p.display()), // send as `Authorization: Bearer <file contents>`
            None => std::process::exit(1),
        },
        _ => eprintln!("{USAGE}"),
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
        let e = p(r#"{"hook_event_name":"PostToolUse","session_id":"s","cwd":"/w/proj","tool_name":"Edit","tool_input":{"file_path":"/w/proj/a.rs"}}"#).unwrap();
        assert_eq!(
            (
                e["event"].as_str(),
                e["path"].as_str(),
                e["workspace"].as_str()
            ),
            (Some("file.changed"), Some("/w/proj/a.rs"), Some("/w/proj"))
        );
        let e = p(r#"{"hook_event_name":"PostToolUse","session_id":"s","tool_name":"Bash","tool_input":{"command":"ls"}}"#).unwrap();
        assert_eq!(
            (e["event"].as_str(), e["tool"].as_str()),
            (Some("tool.completed"), Some("Bash"))
        );
        assert!(p(r#"{"hook_event_name":"PreCompact"}"#).is_none());
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
    fn hooks_merge_without_touching_anything_else() {
        let mut v: Value = serde_json::from_str(r#"{"model":"x","hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"my-linter"}]}],"Stop":[{"hooks":[{"type":"command","command":"acc hook"}]}]}}"#).unwrap();
        merge_hooks(&mut v, "\"/opt/acc\"", "mutating", 120, false);
        let once = v.clone();
        merge_hooks(&mut v, "\"/opt/acc\"", "mutating", 120, false);
        assert_eq!(v, once); // idempotent
        assert_eq!(v["model"], "x");
        assert_eq!(
            v["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "my-linter"
        ); // the user's own hook stays
        assert!(
            v["hooks"]["PreToolUse"][1]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with("hook --gate mutating --timeout 120")
        );
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 1); // the old bare entry was replaced, not duplicated
        assert_eq!(v["hooks"].as_object().unwrap().len(), 7);
        merge_hooks(&mut v, "\"/opt/acc\"", "mutating", 120, true);
        assert_eq!(v["hooks"].as_object().unwrap().len(), 1); // only the user's hook is left
        assert_eq!(
            v["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "my-linter"
        );
    }
}
