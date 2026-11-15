//! Real process control: launch managed agents, pause/resume/stop them, and
//! detect agent CLIs that are already running on the machine.
use crate::log;
use crate::store::{
    Agent, Detected, Error, Severity, Source, Status, Store, SystemInfo, Workspace,
};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

/// Called when something the UI shows has changed.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Sig {
    Pause,
    Resume,
    Stop,
}

/// Pause/resume/stop a process (`group` = its whole process group, for managed
/// shells). Returns false if the OS call failed or isn't supported.
pub fn signal(pid: u32, group: bool, sig: Sig) -> bool {
    if pid <= 1 {
        return false; // never signal init or "every process"
    }
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

/// Signal a known agent and record what was asked. Pause/resume show as "requested" until the
/// process watcher sees the new state; a stop is confirmed when the process exits.
pub fn control(s: &mut Store, id: &str, sig: Sig) -> Result<(), Error> {
    let a = s.agents.get_mut(id).ok_or(Error::UnknownAgent)?;
    let pid = a.pid.ok_or(Error::NotControllable)?;
    if !signal(pid, a.source == Source::Managed, sig) {
        return Err(Error::SignalFailed);
    }
    match sig {
        Sig::Pause => a.requested = Some(Status::Paused),
        Sig::Resume => a.requested = Some(Status::Running),
        Sig::Stop => {
            a.status = Status::Failed;
            a.message = "stopped by you".into();
            a.requested = None;
        }
    }
    let name = a.name.clone();
    s.record(id, &name, &format!("control.{sig:?}").to_lowercase(), "");
    s.touch();
    log::info(
        "control",
        json!({"agent_id": id, "pid": pid, "signal": format!("{sig:?}")}),
    );
    Ok(())
}

/// What is needed to start (or restart) a managed agent.
#[derive(Clone, PartialEq, Debug)]
pub struct Spec {
    pub name: String,
    pub cmd: String,
    pub dir: String,
}

#[derive(Default)]
pub struct Procs {
    children: Vec<(String, Child)>,
    specs: HashMap<String, Spec>,
    pub queue: VecDeque<Spec>, // waiting for a free slot (cfg.max_parallel)
    next: u32,
}

impl Procs {
    pub fn spawn(
        &mut self,
        store: &Arc<Mutex<Store>>,
        wake: &Wake,
        spec: &Spec,
    ) -> Result<String, String> {
        let cmd = spec.cmd.trim();
        if cmd.is_empty() {
            return Err("Enter a command to run".into());
        }
        let dir = spec.dir.trim();
        if !dir.is_empty() && !std::path::Path::new(dir).is_dir() {
            return Err(format!("Working directory does not exist: {dir}"));
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
        c.env("ACC_ADDR", crate::config::addr())
            .env("ACC_URL", format!("http://{}", crate::config::addr()))
            .env("ACC_AGENT_ID", &id);
        c.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !dir.is_empty() {
            c.current_dir(dir);
        }
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut c, 0); // own group so Pause/Stop reach children
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000); // no console window
        let mut child = c.spawn().map_err(|e| format!("Could not start: {e}"))?;

        let name = if spec.name.trim().is_empty() {
            cmd
        } else {
            spec.name.trim()
        };
        let mut a = Agent::new(name, Source::Managed);
        a.task = crate::store::redact(cmd); // shown on the card and returned by the API
        a.pid = Some(child.id());
        a.workspace = dir.into();
        {
            let mut s = store.lock().unwrap();
            s.agents.insert(id.clone(), a);
            s.record(&id, name, "agent.started", cmd);
            s.touch();
        }
        log::info(
            "managed agent started",
            json!({"agent_id": id, "pid": child.id()}),
        );
        let pipes: [Option<Box<dyn std::io::Read + Send>>; 2] = [
            child.stdout.take().map(|p| Box::new(p) as _),
            child.stderr.take().map(|p| Box::new(p) as _),
        ];
        for p in pipes.into_iter().flatten() {
            let (store, wake, id) = (store.clone(), wake.clone(), id.clone());
            std::thread::spawn(move || {
                for line in BufReader::new(p).lines().map_while(Result::ok) {
                    store.lock().unwrap().push_log(&id, line);
                    wake();
                }
            });
        }
        self.specs.insert(id.clone(), spec.clone());
        self.children.push((id.clone(), child));
        Ok(id)
    }

    /// Ids of managed agents that can be restarted.
    pub fn restartable(&self) -> HashSet<String> {
        self.specs.keys().cloned().collect()
    }

    /// The command a managed agent was started with (for Restart).
    pub fn spec(&self, id: &str) -> Option<&Spec> {
        self.specs.get(id)
    }

    /// Start queued launches while there is a free slot.
    pub fn drain_queue(&mut self, store: &Arc<Mutex<Store>>, wake: &Wake) {
        let max = store.lock().unwrap().cfg.max_parallel.max(1);
        while self.children.len() < max
            && let Some(spec) = self.queue.pop_front()
        {
            if let Err(e) = self.spawn(store, wake, &spec) {
                store.lock().unwrap().notify(
                    "queue",
                    format!("Queued task could not start: {}", spec.cmd),
                    e,
                    Severity::Warning,
                );
            }
        }
    }

    /// Notice managed processes that exited on their own (or were stopped). True if any did.
    pub fn reap(&mut self, store: &mut Store) -> bool {
        let before = self.children.len();
        self.children.retain_mut(|(id, child)| {
            let Ok(Some(st)) = child.try_wait() else {
                return true;
            };
            if let Some(a) = store.agents.get_mut(id.as_str()) {
                a.requested = None;
                a.pid = None; // the pid may be reused by an unrelated process: never signal it again
                let mut notice = None;
                if !a.status.finished() {
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
                    let sev = if st.success() {
                        Severity::Info
                    } else {
                        Severity::Critical
                    };
                    notice = Some((
                        format!("{} {}", a.name, a.status.label()),
                        a.message.clone(),
                        sev,
                    ));
                }
                let (n, m) = (a.name.clone(), a.message.clone());
                store.record(id, &n, "agent.exited", &m);
                if let Some((t, b, sev)) = notice {
                    store.notify(id, t, b, sev);
                }
                store.touch();
            }
            false
        });
        self.children.len() != before
    }

    pub fn running(&self) -> usize {
        self.children.len()
    }

    /// Stop every managed process group. Called when the HUD quits so nothing is left orphaned.
    pub fn stop_all(&mut self) {
        for (id, child) in &mut self.children {
            signal(child.id(), true, Sig::Stop);
            log::info("managed agent stopped at shutdown", json!({"agent_id": id}));
        }
        std::thread::sleep(Duration::from_millis(200));
        for (_, child) in &mut self.children {
            if let Ok(None) = child.try_wait() {
                let _ = child.kill();
            }
        }
        self.children.clear();
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

fn git_status(dir: &str) -> Workspace {
    let out = Command::new("git")
        .args(["-C", dir, "status", "--porcelain=v1", "-b"])
        .stdin(Stdio::null())
        .output();
    let Ok(o) = out else {
        return Workspace::default();
    };
    if !o.status.success() {
        return Workspace::default(); // not a git repository
    }
    let text = String::from_utf8_lossy(&o.stdout);
    let mut lines = text.lines();
    let branch = lines
        .next()
        .unwrap_or("")
        .trim_start_matches("## ")
        .split("...")
        .next()
        .unwrap_or("")
        .to_string();
    Workspace {
        branch,
        changed: lines
            .take(50)
            .map(|l| l.get(3..).unwrap_or(l).to_string())
            .collect(),
        git: true,
    }
}

/// Background thread: every few seconds refresh detected agents, system load, pause confirmations,
/// resource alerts and (less often) the git state of each workspace. Wakes the UI only on change.
pub fn watch(store: Arc<Mutex<Store>>, wake: Wake) {
    use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};
    std::thread::spawn(move || {
        let Ok(mut sys) = System::new() else {
            store.lock().unwrap().scanned = true; // unsupported platform: no detection
            return;
        };
        let me = std::process::id();
        // Only what the cards show. Skipping per-thread tasks, environment, cwd and disk I/O keeps a scan cheap.
        let kind = ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .without_tasks();
        let mut hot: HashSet<u32> = HashSet::new(); // pids already reported as over a threshold
        let mut tick = 0u32;
        loop {
            sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
            sys.refresh_cpu_usage();
            sys.refresh_memory();
            let system = SystemInfo {
                cpu: sys.global_cpu_usage().round(),
                mem_used_mb: sys.used_memory() / 1_048_576,
                mem_total_mb: sys.total_memory() / 1_048_576,
            };
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
                    cmd: crate::store::redact(&cmd.join(" "))
                        .chars()
                        .take(90)
                        .collect(),
                    cpu: p.cpu_usage().round(),
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

            let dirs: Vec<String> = {
                let mut s = store.lock().unwrap();
                let mut changed = s.detected != found || s.system != system || !s.scanned;
                // Confirm requested pauses/resumes against what the OS reports.
                for a in s.agents.values_mut() {
                    let (Some(want), Some(pid)) = (a.requested, a.pid) else {
                        continue;
                    };
                    let stopped = sys
                        .process(sysinfo::Pid::from_u32(pid))
                        .is_some_and(|p| p.status() == ProcessStatus::Stop);
                    if stopped == (want == Status::Paused) {
                        a.status = want;
                        a.requested = None;
                        changed = true;
                    }
                }
                // Resource alerts: once per process until it drops back under the limit.
                let (cpu_max, mem_max) = (s.cfg.cpu_alert, s.cfg.mem_alert_mb);
                let mut over = HashSet::new();
                let mut alerts = vec![];
                for d in &found {
                    let why = if cpu_max > 0.0 && d.cpu > cpu_max {
                        format!("CPU {:.0}% (limit {cpu_max:.0}%)", d.cpu)
                    } else if mem_max > 0 && d.mem_mb > mem_max {
                        format!("{} MB of memory (limit {mem_max} MB)", d.mem_mb)
                    } else {
                        continue;
                    };
                    over.insert(d.pid);
                    if !hot.contains(&d.pid) {
                        alerts.push((
                            format!("{} (pid {}) is using a lot of resources", d.label, d.pid),
                            why,
                        ));
                    }
                }
                hot = over;
                for (t, b) in alerts {
                    s.notify("system", t, b, Severity::Warning);
                    changed = true;
                }
                s.detected = found;
                s.system = system;
                s.scanned = true;
                if changed {
                    wake();
                }
                s.agents
                    .values()
                    .filter(|a| !a.workspace.is_empty())
                    .map(|a| a.workspace.clone())
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .take(12)
                    .collect()
            };
            if tick.is_multiple_of(5) {
                // git runs outside the lock: it can be slow on a big repository
                let ws: BTreeMap<String, Workspace> =
                    dirs.iter().map(|d| (d.clone(), git_status(d))).collect();
                let mut s = store.lock().unwrap();
                if s.workspaces != ws {
                    s.workspaces = ws;
                    wake();
                }
            }
            tick = tick.wrapping_add(1);
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
    fn launch_pause_resume_stop_restart_and_shutdown() {
        let wake: Wake = Arc::new(|| {});
        let store = Arc::new(Mutex::new(Store::default()));
        let mut p = Procs::default();
        let spec = Spec {
            name: "".into(),
            cmd: "echo hello; echo --password hunter2; sleep 30".into(),
            dir: "".into(),
        };
        assert!(
            p.spawn(
                &store,
                &wake,
                &Spec {
                    dir: "/no/such/dir".into(),
                    ..spec.clone()
                }
            )
            .is_err()
        );
        let id = p.spawn(&store, &wake, &spec).unwrap();
        let pid = store.lock().unwrap().agents[&id].pid.unwrap();
        std::thread::sleep(Duration::from_millis(400));
        {
            let s = store.lock().unwrap();
            assert_eq!(s.agents[&id].log.front().map(String::as_str), Some("hello"));
            assert!(!s.agents[&id].log[1].contains("hunter2")); // output is scrubbed
            assert_eq!(s.history.back().unwrap().agent, spec.cmd); // unnamed agents are filed under their command
        }
        {
            let mut s = store.lock().unwrap();
            control(&mut s, &id, Sig::Pause).unwrap();
            assert_eq!(
                (s.agents[&id].status, s.agents[&id].requested),
                (Status::Running, Some(Status::Paused))
            ); // asked, not yet confirmed
            control(&mut s, &id, Sig::Resume).unwrap();
            control(&mut s, &id, Sig::Stop).unwrap();
            assert_eq!(
                control(&mut s, "nobody", Sig::Stop),
                Err(Error::UnknownAgent)
            );
        }
        std::thread::sleep(Duration::from_millis(500));
        {
            let mut s = store.lock().unwrap();
            assert!(p.reap(&mut s));
            assert_eq!(
                (s.agents[&id].status, s.agents[&id].pid),
                (Status::Failed, None)
            ); // killed; pid forgotten
            assert_eq!(control(&mut s, &id, Sig::Stop), Err(Error::NotControllable));
        }
        assert!(!signal(pid, true, Sig::Stop)); // the group is gone
        // restart from the remembered spec, then shut down: nothing may be left running
        let again = p.spec(&id).unwrap().clone();
        assert_eq!(again, spec);
        let id2 = p.spawn(&store, &wake, &again).unwrap();
        let pid2 = store.lock().unwrap().agents[&id2].pid.unwrap();
        p.stop_all();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!signal(pid2, true, Sig::Pause));
        assert_eq!(p.running(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn queue_respects_the_parallel_limit() {
        let wake: Wake = Arc::new(|| {});
        let store = Arc::new(Mutex::new(Store::default()));
        store.lock().unwrap().cfg.max_parallel = 1;
        let mut p = Procs::default();
        for n in ["one", "two"] {
            p.queue.push_back(Spec {
                name: n.into(),
                cmd: "sleep 0.3".into(),
                dir: "".into(),
            });
        }
        p.drain_queue(&store, &wake);
        assert_eq!((p.running(), p.queue.len()), (1, 1));
        std::thread::sleep(Duration::from_millis(600));
        p.reap(&mut store.lock().unwrap());
        p.drain_queue(&store, &wake);
        assert_eq!((p.running(), p.queue.len()), (1, 0));
        p.stop_all();
    }

    #[test]
    fn git_status_of_this_repository() {
        let w = git_status(env!("CARGO_MANIFEST_DIR"));
        assert!(w.git && !w.branch.is_empty());
        assert!(!git_status("/").git);
    }
}
