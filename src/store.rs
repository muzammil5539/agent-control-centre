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
const HISTORY_CAP: usize = 2000;
pub const APPROVAL_TTL: u32 = 300; // seconds a pending approval stays open
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
    pub approval_id: Option<String>, // unique per request; the agent polls /approvals/<id>
    pub detail: Option<String>,      // the exact action awaiting approval (shown in full)
    pub expires_in: Option<u32>,     // approval lifetime in seconds
    pub notify: Option<bool>,        // force a native notification for this event
}

/// A request for the user's permission. Bound to one exact action; never auto-approved.
pub struct Approval {
    pub id: String,
    pub agent_id: String,
    pub agent: String,
    pub message: String,
    pub detail: String,
    pub created: Instant,
    pub ttl: Duration,
    pub answer: Option<bool>,
    pub expired: bool,
}

impl Approval {
    pub fn pending(&self) -> bool {
        self.answer.is_none() && !self.expired
    }
    pub fn status(&self) -> &'static str {
        match (self.answer, self.expired) {
            (Some(true), _) => "approved",
            (Some(false), _) => "denied",
            (None, true) => "expired",
            _ => "pending",
        }
    }
    pub fn remaining(&self) -> Duration {
        self.ttl.saturating_sub(self.created.elapsed())
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
    pub ts: u64,
    #[serde(default)]
    pub agent_id: String,
    pub agent: String,
    pub event: String,
    pub message: String,
}

#[derive(Default)]
pub struct Store {
    pub agents: BTreeMap<String, Agent>,
    pub detected: Vec<Detected>,
    pub history: VecDeque<Entry>,
    pub approvals: Vec<Approval>,
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

    pub fn record(&mut self, agent_id: &str, agent: &str, event: &str, message: &str) {
        if self
            .history
            .back()
            .is_some_and(|l| l.agent_id == agent_id && l.event == event && l.message == message)
        {
            return; // identical consecutive entry
        }
        let e = Entry {
            ts: now_secs(),
            agent_id: agent_id.into(),
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

    fn pending_for(&self, agent_id: &str) -> bool {
        self.approvals
            .iter()
            .any(|x| x.agent_id == agent_id && x.pending())
    }

    /// Apply one event. Err = invalid event. Ok(Some) = user should be notified.
    pub fn apply(&mut self, e: Event) -> Result<Option<Notice>, &'static str> {
        const KINDS: [&str; 7] = [
            "agent.started",
            "agent.progress",
            "agent.waiting",
            "approval.required",
            "agent.completed",
            "agent.failed",
            "agent.heartbeat",
        ];
        if !KINDS.contains(&e.event.as_str()) {
            return Err("unknown event type");
        }
        let approval_id = e.approval_id.clone().unwrap_or_else(|| e.agent_id.clone());
        if e.event == "approval.required" {
            if self
                .approvals
                .iter()
                .any(|x| x.id == approval_id && x.pending())
            {
                return Ok(None); // retry/duplicate of a request that is still open
            }
            self.approvals.retain(|x| x.id != approval_id);
        }
        let pending = self.pending_for(&e.agent_id);
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
            _ if old == Status::Lost => Status::Running, // heartbeat after silence
            _ => old,
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
        // Paused sticks until the user resumes; an open approval sticks until answered.
        a.status = match (old, new) {
            (Status::Paused, Status::Running) => old,
            (_, Status::Running) if pending => Status::Approval,
            _ => new,
        };
        let (name, msg, task) = (a.name.clone(), a.message.clone(), a.task.clone());
        let detail = e.detail.unwrap_or_default();
        if new == Status::Approval {
            self.approvals.push(Approval {
                id: approval_id,
                agent_id: e.agent_id.clone(),
                agent: name.clone(),
                message: msg.clone(),
                detail: detail.clone(),
                created: Instant::now(),
                ttl: Duration::from_secs(
                    e.expires_in.unwrap_or(APPROVAL_TTL).clamp(5, 86_400) as u64
                ),
                answer: None,
                expired: false,
            });
            if let Some(a) = self.agents.get_mut(&e.agent_id) {
                a.approval = None;
            }
        }
        if e.event != "agent.heartbeat" {
            let text = match e.event.as_str() {
                "approval.required" if !detail.is_empty() => detail.clone(),
                "agent.progress" | "agent.started" if !task.is_empty() => task.clone(),
                _ if !msg.is_empty() => msg.clone(),
                _ => task.clone(),
            };
            self.record(&e.agent_id, &name, &e.event, &text);
        }
        let notify = (new != old
            && matches!(new, Status::Approval | Status::Done | Status::Failed))
            || e.notify == Some(true);
        Ok(notify.then(|| {
            let what = match new {
                Status::Approval => "needs your approval",
                Status::Done => "finished",
                Status::Waiting => "needs attention",
                _ => "failed",
            };
            (
                format!("{name} {what}"),
                if detail.is_empty() { msg } else { detail },
            )
        }))
    }

    /// Lost-heartbeat detection (ttl * scale) and approval expiry. Returns notices for newly lost agents.
    pub fn sweep(&mut self, scale: f32) -> Vec<Notice> {
        let mut lost = vec![];
        for a in self.agents.values_mut() {
            if a.source == Source::Api
                && matches!(a.status, Status::Running | Status::Waiting)
                && a.seen.elapsed() > a.ttl.mul_f32(scale)
            {
                a.status = Status::Lost;
                lost.push((a.name.clone(), a.name.clone()));
            }
        }
        let expired: Vec<(String, String, String)> = self
            .approvals
            .iter_mut()
            .filter(|x| x.pending() && x.created.elapsed() > x.ttl.mul_f32(scale))
            .map(|x| {
                x.expired = true;
                (x.agent_id.clone(), x.agent.clone(), x.detail.clone())
            })
            .collect();
        for (id, name, detail) in expired {
            self.settle(&id);
            self.record(&id, &name, "approval.expired", &detail);
        }
        lost.into_iter()
            .map(|(n, _)| {
                self.record(&n, &n, "agent.heartbeat_lost", "no heartbeat");
                (format!("{n} stopped responding"), "heartbeat lost".into())
            })
            .collect()
    }

    /// After an approval resolves: an agent with nothing else pending goes back to running.
    fn settle(&mut self, agent_id: &str) {
        let pending = self.pending_for(agent_id);
        if let Some(a) = self.agents.get_mut(agent_id) {
            a.seen = Instant::now(); // it was blocked on the user, not silent
            if a.status == Status::Approval && !pending {
                a.status = Status::Running;
            }
        }
        // keep every pending approval plus the 100 most recent resolved ones
        let resolved = self.approvals.iter().filter(|x| !x.pending()).count();
        if resolved > 100 {
            let mut skip = resolved - 100;
            self.approvals.retain(|x| {
                x.pending() || {
                    let keep = skip == 0;
                    skip = skip.saturating_sub(1);
                    keep
                }
            });
        }
    }

    /// Record the user's decision for one approval request.
    pub fn answer(&mut self, approval_id: &str, ok: bool) {
        let Some(ap) = self
            .approvals
            .iter_mut()
            .find(|x| x.id == approval_id && x.pending())
        else {
            return;
        };
        ap.answer = Some(ok);
        let (id, name, detail) = (ap.agent_id.clone(), ap.agent.clone(), ap.detail.clone());
        if let Some(a) = self.agents.get_mut(&id) {
            a.approval = Some(ok);
        }
        self.settle(&id);
        self.record(
            &id,
            &name,
            if ok {
                "approval.granted"
            } else {
                "approval.denied"
            },
            &detail,
        );
    }

    pub fn pending_approvals(&self) -> impl Iterator<Item = &Approval> {
        self.approvals.iter().filter(|x| x.pending())
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
            approval_id: None,
            detail: None,
            expires_in: None,
            notify: None,
        }
    }

    fn ask(id: &str, aid: &str, detail: &str) -> Event {
        let mut e = ev("approval.required", id);
        e.approval_id = Some(aid.into());
        e.detail = Some(detail.into());
        e
    }

    #[test]
    fn lifecycle() {
        let mut s = Store::default();
        assert!(s.apply(ev("agent.started", "a")).unwrap().is_none()); // no noise on start
        assert!(s.apply(ask("a", "r1", "rm -rf build")).unwrap().is_some());
        assert!(s.apply(ask("a", "r1", "rm -rf build")).unwrap().is_none()); // retry of open request: no spam, no duplicate
        assert_eq!(s.approvals.len(), 1);
        assert_eq!(s.agents["a"].status, Status::Approval);
        s.answer("r1", true);
        assert_eq!(
            (s.agents["a"].status, s.agents["a"].approval),
            (Status::Running, Some(true))
        );
        s.answer("r1", false); // an answered approval can't be flipped or replayed
        assert_eq!(s.approvals[0].status(), "approved");
        assert!(s.apply(ev("agent.completed", "a")).unwrap().is_some());
        assert_eq!(s.agents["a"].progress, 100);
        assert!(s.apply(ev("bogus", "a")).is_err());
    }

    #[test]
    fn approvals_are_independent_and_never_auto_approved() {
        let mut s = Store::default();
        s.apply(ask("a", "r1", "git push")).unwrap();
        s.apply(ask("a", "r2", "rm -rf /tmp/x")).unwrap();
        s.apply(ev("agent.progress", "a")).unwrap(); // progress must not hide an open approval
        assert_eq!(s.agents["a"].status, Status::Approval);
        s.answer("r1", false);
        assert_eq!(s.agents["a"].status, Status::Approval); // r2 still open
        assert_eq!(s.approvals[0].status(), "denied");
        assert_eq!(s.approvals[1].status(), "pending");
        s.sweep(0.0); // expire everything
        assert_eq!(s.approvals[1].status(), "expired"); // timeout means expired, never approved
        assert_eq!(s.agents["a"].status, Status::Running); // waiting on the user must not read as "lost"
        assert!(s.agents["a"].seen.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn history_is_per_agent() {
        let mut s = Store::default();
        let mut e = ev("agent.progress", "a");
        e.task = Some("Bash cargo test".into());
        s.apply(e).unwrap();
        s.apply(ev("agent.started", "b")).unwrap();
        assert_eq!(
            s.history
                .iter()
                .filter(|h| h.agent_id == "a" && h.message == "Bash cargo test")
                .count(),
            1
        );
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
