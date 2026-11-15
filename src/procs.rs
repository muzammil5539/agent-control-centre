//! Real process control: launch managed agents, pause/resume/stop them, and
//! detect agent CLIs that are already running on the machine.
use crate::store::{ADDR, Agent, Detected, Notice, Source, Status, Store};
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Copy, PartialEq)]
pub enum Sig {
    Pause,
    Resume,
    Stop,
}

/// Pause/resume/stop a process (`group` = its whole process group, for managed
/// shells). Returns false if the OS call failed or isn't supported.
pub fn signal(pid: u32, group: bool, sig: Sig) -> bool {
    if cfg!(windows) {
        // Windows has no portable suspend; only stopping is supported.
        return sig == Sig::Stop
            && Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output()
                .is_ok_and(|o| o.status.success());
    }
    let target = format!("{}{pid}", if group { "-" } else { "" });
    let send = |s: &str| {
        Command::new("kill")
            .args([s, "--", &target])
            .output()
            .is_ok_and(|o| o.status.success())
    };
    match sig {
        Sig::Pause => send("-STOP"),
        Sig::Resume => send("-CONT"),
        Sig::Stop => {
            let ok = send("-TERM");
            send("-CONT"); // a stopped process only handles TERM once continued
            ok
        }
    }
}

pub const CAN_PAUSE: bool = !cfg!(windows);

#[derive(Default)]
pub struct Procs {
    children: Vec<(String, Child)>,
    next: u32,
}

impl Procs {
    pub fn spawn(
        &mut self,
        store: &Arc<Mutex<Store>>,
        ctx: &eframe::egui::Context,
        name: &str,
        cmd: &str,
        dir: &str,
    ) -> Result<(), String> {
        if cmd.trim().is_empty() {
            return Err("Enter a command to run".into());
        }
        self.next += 1;
        let id = format!("managed-{}", self.next);
        let mut c = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", cmd]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", cmd]);
            c
        };
        c.env("ACC_URL", format!("http://{ADDR}"))
            .env("ACC_AGENT_ID", &id);
        c.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !dir.trim().is_empty() {
            c.current_dir(dir.trim());
        }
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut c, 0); // own group so Pause/Stop reach children
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000); // no console window
        let mut child = c.spawn().map_err(|e| format!("Could not start: {e}"))?;

        let mut a = Agent::new(
            if name.trim().is_empty() {
                cmd
            } else {
                name.trim()
            },
            Source::Managed,
        );
        a.task = cmd.into();
        a.pid = Some(child.id());
        {
            let mut s = store.lock().unwrap();
            s.agents.insert(id.clone(), a);
            s.record(&id, name, "agent.started", cmd);
        }
        let pipes: [Option<Box<dyn std::io::Read + Send>>; 2] = [
            child.stdout.take().map(|p| Box::new(p) as _),
            child.stderr.take().map(|p| Box::new(p) as _),
        ];
        for p in pipes.into_iter().flatten() {
            let (store, ctx, id) = (store.clone(), ctx.clone(), id.clone());
            std::thread::spawn(move || {
                for line in BufReader::new(p).lines().map_while(Result::ok) {
                    store.lock().unwrap().push_log(&id, line);
                    ctx.request_repaint();
                }
            });
        }
        self.children.push((id, child));
        Ok(())
    }

    /// Notice managed processes that exited on their own (or were stopped).
    pub fn reap(&mut self, store: &mut Store) -> Vec<Notice> {
        let mut out = vec![];
        self.children.retain_mut(|(id, child)| {
            let Ok(Some(st)) = child.try_wait() else {
                return true;
            };
            if let Some(a) = store.agents.get_mut(id.as_str()) {
                if !matches!(a.status, Status::Done | Status::Failed) {
                    a.status = if st.success() {
                        Status::Done
                    } else {
                        Status::Failed
                    };
                    a.message = match st.code() {
                        Some(c) => format!("exited with code {c}"),
                        None => "terminated".into(),
                    };
                    if st.success() {
                        a.progress = 100;
                    }
                    out.push((
                        format!("{} {}", a.name, a.status.label()),
                        a.message.clone(),
                    ));
                }
                let (n, m) = (a.name.clone(), a.message.clone());
                store.record(id, &n, "agent.exited", &m);
            }
            false
        });
        out
    }

    pub fn running(&self) -> usize {
        self.children.len()
    }
}

// ---- detection of agent CLIs already running on the machine ----

/// (token matched against executable / script names, display label)
const KNOWN: &[(&str, &str)] = &[
    ("claude", "Claude Code"),
    ("codex", "Codex CLI"),
    ("gemini", "Gemini CLI"),
    ("aider", "Aider"),
    ("opencode", "OpenCode"),
    ("goose", "Goose"),
    ("cursor-agent", "Cursor Agent"),
    ("langgraph", "LangGraph"),
    ("crewai", "CrewAI"),
];

fn stem(s: &str) -> &str {
    let base = s.rsplit(['/', '\\']).next().unwrap_or(s);
    base.strip_suffix(".exe")
        .or_else(|| base.strip_suffix(".js"))
        .or_else(|| base.strip_suffix(".py"))
        .unwrap_or(base)
}

/// Best-effort: is this process an AI agent CLI? Matches the executable name or
/// one of the first args (e.g. `node /usr/bin/claude`, `python -m aider`).
pub fn agent_label(name: &str, cmd: &[String]) -> Option<&'static str> {
    if cmd.iter().any(|a| a.starts_with("--type=")) {
        return None; // electron/chromium helper processes
    }
    let joined = cmd.join(" ").to_lowercase();
    if joined.contains("claude-desktop")
        || joined.contains("/claude.app/")
        || joined.contains("agent-control-centre")
    {
        return None; // the desktop chat app is not the CLI agent; and never match ourselves
    }
    let words: Vec<&str> = std::iter::once(name)
        .chain(cmd.iter().take(3).map(String::as_str))
        .collect();
    KNOWN
        .iter()
        .find(|(tok, _)| words.iter().any(|w| stem(w) == *tok))
        .map(|(_, l)| *l)
}

/// Background thread: every few seconds refresh the list of detected agents.
pub fn watch(store: Arc<Mutex<Store>>, ctx: eframe::egui::Context) {
    use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
    std::thread::spawn(move || {
        let Ok(mut sys) = System::new() else { return }; // unsupported platform: no detection
        let me = std::process::id();
        loop {
            sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::everything(),
            );
            let managed: Vec<u32> = store
                .lock()
                .unwrap()
                .agents
                .values()
                .filter_map(|a| a.pid)
                .collect();
            let mut found: Vec<Detected> = vec![];
            for (pid, p) in sys.processes() {
                let cmd: Vec<String> = p
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect();
                let Some(label) = agent_label(&p.name().to_string_lossy(), &cmd) else {
                    continue;
                };
                let parent = p.parent().map(|x| x.as_u32());
                if pid.as_u32() == me
                    || managed.contains(&pid.as_u32())
                    || parent.is_some_and(|x| managed.contains(&x))
                {
                    continue;
                }
                found.push(Detected {
                    pid: pid.as_u32(),
                    label: label.into(),
                    cmd: cmd.join(" ").chars().take(90).collect(),
                    cpu: p.cpu_usage(),
                    mem_mb: p.memory() / 1_048_576,
                    paused: p.status() == ProcessStatus::Stop,
                });
            }
            // Show only top-level processes: drop any whose parent is also detected.
            let pids: Vec<u32> = found.iter().map(|d| d.pid).collect();
            found.retain(|d| {
                let parent = sys
                    .process(sysinfo::Pid::from_u32(d.pid))
                    .and_then(|p| p.parent())
                    .map(|x| x.as_u32());
                !parent.is_some_and(|x| pids.contains(&x))
            });
            found.sort_by_key(|d| d.pid);
            store.lock().unwrap().detected = found;
            ctx.request_repaint();
            std::thread::sleep(Duration::from_secs(3));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_agent_clis() {
        assert_eq!(agent_label("claude", &v(&["claude"])), Some("Claude Code"));
        assert_eq!(
            agent_label("node", &v(&["node", "/usr/lib/node_modules/@openai/codex"])),
            Some("Codex CLI")
        );
        assert_eq!(
            agent_label("python3", &v(&["python3", "-m", "aider"])),
            Some("Aider")
        );
        assert_eq!(
            agent_label("claude", &v(&["/opt/Claude/claude", "--type=renderer"])),
            None
        );
        assert_eq!(agent_label("bash", &v(&["bash", "-c", "ls"])), None);
    }

    #[cfg(unix)]
    #[test]
    fn launch_pause_resume_stop() {
        let ctx = eframe::egui::Context::default();
        let store = Arc::new(Mutex::new(Store::default()));
        let mut p = Procs::default();
        p.spawn(&store, &ctx, "t", "echo hello; sleep 30", "")
            .unwrap();
        let pid = store.lock().unwrap().agents["managed-1"].pid.unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(
            store.lock().unwrap().agents["managed-1"]
                .log
                .front()
                .map(String::as_str),
            Some("hello")
        );
        if CAN_PAUSE {
            assert!(signal(pid, true, Sig::Pause));
            assert!(signal(pid, true, Sig::Resume));
        }
        assert!(signal(pid, true, Sig::Stop));
        std::thread::sleep(Duration::from_millis(500));
        let mut s = store.lock().unwrap();
        assert_eq!(p.reap(&mut s).len(), 1);
        assert_eq!(s.agents["managed-1"].status, Status::Failed); // killed by signal
    }
}
