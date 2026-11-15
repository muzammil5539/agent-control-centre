//! Agent registry, event handling and persisted activity history.
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    io::Write,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const ADDR: &str = "127.0.0.1:7777"; // localhost only
pub const DEFAULT_TTL: u32 = 30; // seconds of silence before an API agent is "lost"
const HISTORY_CAP: usize = 300;
const LOG_CAP: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Running,
    Waiting,
    Approval,
    Paused,
    Done,
    Failed,
    Lost,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Waiting => "waiting",
            Status::Approval => "needs approval",
            Status::Paused => "paused",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Lost => "lost",
        }
    }
    /// Short machine name used by the HTTP API.
    pub fn api(self) -> &'static str {
        match self {
            Status::Approval => "approval",
            s => s.label(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Api,     // reported itself over HTTP
    Managed, // launched from the HUD
}

pub struct Agent {
    pub name: String,
    pub task: String,
    pub message: String,
    pub status: Status,
    pub progress: u8,
    pub seen: Instant,
    pub ttl: Duration,
    pub approval: Option<bool>, // answer to the last approval.required; agents poll for it
    pub pid: Option<u32>,
    pub source: Source,
    pub log: VecDeque<String>,
}

impl Agent {
    pub fn new(name: &str, source: Source) -> Self {
        Agent {
            name: name.into(),
            task: String::new(),
            message: String::new(),
            status: Status::Running,
            progress: 0,
            seen: Instant::now(),
            ttl: Duration::from_secs(DEFAULT_TTL as u64),
            approval: None,
            pid: None,
            source,
            log: VecDeque::new(),
        }
    }
}

/// A process we recognised as an AI agent without it reporting anything.
pub struct Detected {
    pub pid: u32,
    pub label: String,
    pub cmd: String,
    pub cpu: f32,
    pub mem_mb: u64,
    pub paused: bool,
}

#[derive(Deserialize)]
pub struct Event {
    pub event: String,
    pub agent_id: String,
    pub name: Option<String>,
    pub task: Option<String>,
    pub message: Option<String>,
    pub progress: Option<u32>,
    pub pid: Option<u32>,
    pub ttl: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
    pub ts: u64,
    pub agent: String,
    pub event: String,
    pub message: String,
}

#[derive(Default)]
pub struct Store {
    pub agents: BTreeMap<String, Agent>,
    pub detected: Vec<Detected>,
    pub history: VecDeque<Entry>,
    history_file: Option<PathBuf>,
}

pub type Notice = (String, String); // (title, body) for a native notification

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn data_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA")?.into()
    } else if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(x)
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".local/share")
    };
    Some(base.join("agent-control-centre"))
}

impl Store {
    /// Store backed by the on-disk history (last entries are reloaded).
    pub fn persistent() -> Self {
        let mut s = Store::default();
        let Some(dir) = data_dir() else { return s };
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("history.jsonl");
        let lines: Vec<String> = std::fs::read_to_string(&file)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        let keep = &lines[lines.len().saturating_sub(HISTORY_CAP)..];
        if lines.len() > 4 * HISTORY_CAP {
            // trim the file so it doesn't grow without bound
            let _ = std::fs::write(&file, keep.join("\n") + "\n");
        }
        s.history = keep
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        s.history_file = Some(file);
        s
    }

    pub fn record(&mut self, agent: &str, event: &str, message: &str) {
        let e = Entry {
            ts: now_secs(),
            agent: agent.into(),
            event: event.into(),
            message: message.into(),
        };
        if let Some(f) = &self.history_file
            && let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(f)
        {
            let _ = writeln!(f, "{}", serde_json::to_string(&e).unwrap_or_default());
        }
        self.history.push_back(e);
        if self.history.len() > HISTORY_CAP {
            self.history.pop_front();
        }
    }

    /// Apply one event. Err = unknown event type. Ok(Some) = user should be notified.
    pub fn apply(&mut self, e: Event) -> Result<Option<Notice>, &'static str> {
        let a = self
            .agents
            .entry(e.agent_id.clone())
            .or_insert_with(|| Agent::new(&e.agent_id, Source::Api));
        let old = a.status;
        let new = match e.event.as_str() {
            "agent.started" | "agent.progress" => Status::Running,
            "agent.waiting" => Status::Waiting,
            "approval.required" => Status::Approval,
            "agent.completed" => Status::Done,
            "agent.failed" => Status::Failed,
            "agent.heartbeat" if old == Status::Lost => Status::Running,
            "agent.heartbeat" => old,
            _ => return Err("unknown event type"),
        };
        a.seen = Instant::now();
        a.name = e.name.unwrap_or_else(|| a.name.clone());
        a.task = e.task.unwrap_or_else(|| a.task.clone());
        a.message = e.message.unwrap_or_else(|| a.message.clone());
        a.pid = e.pid.or(a.pid);
        if let Some(t) = e.ttl {
            a.ttl = Duration::from_secs(t.clamp(5, 86_400) as u64);
        }
        if let Some(p) = e.progress {
            a.progress = p.min(100) as u8;
        }
        if new == Status::Done {
            a.progress = 100;
        }
        if new == Status::Approval {
            a.approval = None;
        }
        // A paused agent stays paused until the user resumes it, whatever it reports.
        a.status = if old == Status::Paused && new == Status::Running {
            old
        } else {
            new
        };
        let (name, msg, task) = (a.name.clone(), a.message.clone(), a.task.clone());
        let notify = new != old && matches!(new, Status::Approval | Status::Done | Status::Failed);
        if e.event != "agent.heartbeat" && e.event != "agent.progress" {
            self.record(&name, &e.event, if msg.is_empty() { &task } else { &msg });
        }
        Ok(notify.then(|| {
            let what = match new {
                Status::Approval => "needs your approval",
                Status::Done => "finished",
                _ => "failed",
            };
            (format!("{name} {what}"), msg)
        }))
    }

    /// Mark silent API agents as lost (ttl * scale); returns notices for the newly lost.
    pub fn sweep(&mut self, scale: f32) -> Vec<Notice> {
        let mut lost = vec![];
        for a in self.agents.values_mut() {
            if a.source == Source::Api
                && matches!(a.status, Status::Running | Status::Waiting)
                && a.seen.elapsed() > a.ttl.mul_f32(scale)
            {
                a.status = Status::Lost;
                lost.push(a.name.clone());
            }
        }
        lost.into_iter()
            .map(|n| {
                self.record(&n, "agent.heartbeat_lost", "no heartbeat");
                (format!("{n} stopped responding"), "heartbeat lost".into())
            })
            .collect()
    }

    pub fn answer(&mut self, id: &str, ok: bool) {
        let Some(a) = self.agents.get_mut(id) else {
            return;
        };
        a.approval = Some(ok);
        a.status = if ok { Status::Running } else { Status::Failed };
        let (name, msg) = (a.name.clone(), if ok { "approved" } else { "denied" });
        if !ok {
            a.message = "approval denied".into();
        }
        self.record(
            &name,
            if ok {
                "approval.granted"
            } else {
                "approval.denied"
            },
            msg,
        );
    }

    pub fn push_log(&mut self, id: &str, line: String) {
        if let Some(a) = self.agents.get_mut(id) {
            a.seen = Instant::now();
            a.log.push_back(line);
            if a.log.len() > LOG_CAP {
                a.log.pop_front();
            }
        }
    }

    pub fn count(&self, s: Status) -> usize {
        self.agents.values().filter(|a| a.status == s).count()
            + match s {
                Status::Running => self.detected.iter().filter(|d| !d.paused).count(),
                Status::Paused => self.detected.iter().filter(|d| d.paused).count(),
                _ => 0,
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn ev(kind: &str, id: &str) -> Event {
        Event {
            event: kind.into(),
            agent_id: id.into(),
            name: None,
            task: None,
            message: None,
            progress: None,
            pid: None,
            ttl: None,
        }
    }

    #[test]
    fn lifecycle() {
        let mut s = Store::default();
        assert!(s.apply(ev("agent.started", "a")).unwrap().is_none()); // no noise on start
        assert!(s.apply(ev("approval.required", "a")).unwrap().is_some());
        assert!(s.apply(ev("approval.required", "a")).unwrap().is_none()); // no repeat spam
        s.answer("a", true);
        assert_eq!(
            (s.agents["a"].status, s.agents["a"].approval),
            (Status::Running, Some(true))
        );
        assert!(s.apply(ev("agent.completed", "a")).unwrap().is_some());
        assert_eq!(s.agents["a"].progress, 100);
        assert!(s.apply(ev("bogus", "a")).is_err());
        assert!(s.history.len() >= 4); // started, approval x2, granted, completed all logged
    }

    #[test]
    fn heartbeat_lost_and_recovered() {
        let mut s = Store::default();
        s.apply(ev("agent.started", "a")).unwrap();
        assert_eq!(s.sweep(0.0).len(), 1);
        assert_eq!(s.agents["a"].status, Status::Lost);
        assert!(s.sweep(0.0).is_empty()); // notified once only
        s.apply(ev("agent.heartbeat", "a")).unwrap();
        assert_eq!(s.agents["a"].status, Status::Running);
    }

    #[test]
    fn paused_stays_paused_and_ttl_applies() {
        let mut s = Store::default();
        let mut e = ev("agent.started", "a");
        e.ttl = Some(900);
        s.apply(e).unwrap();
        assert_eq!(s.agents["a"].ttl, Duration::from_secs(900));
        s.agents.get_mut("a").unwrap().status = Status::Paused;
        s.apply(ev("agent.progress", "a")).unwrap();
        assert_eq!(s.agents["a"].status, Status::Paused);
    }
}
