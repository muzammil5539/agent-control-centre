//! Agent registry, event handling, approvals, notifications and persisted history.
pub use crate::config::Severity;
use crate::config::{Config, data_dir};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_TTL: u32 = 30; // seconds of silence before an API agent is "lost"
pub const PROTOCOL_VERSION: u32 = 1;
pub const APPROVAL_TTL: u32 = 300; // seconds a pending approval stays open
const HISTORY_CAP: usize = 2000;
const NOTES_CAP: usize = 300;
const SEEN_CAP: usize = 1024; // remembered event ids
const LOG_CAP: usize = 400; // output lines kept per managed agent
const AGENT_CAP: usize = 500;
const CONFLICT_WINDOW: u64 = 600; // two agents touching one file within this many seconds

/// What the agent is doing (reported by the agent or the user's controls).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Waiting,
    Approval,
    Paused,
    Done,
    Failed,
}

impl Status {
    pub const ALL: [Status; 6] = [
        Status::Running,
        Status::Waiting,
        Status::Approval,
        Status::Paused,
        Status::Done,
        Status::Failed,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Waiting => "waiting",
            Status::Approval => "needs approval",
            Status::Paused => "paused",
            Status::Done => "done",
            Status::Failed => "failed",
        }
    }
    /// Short machine name used by the HTTP API.
    pub fn api(self) -> &'static str {
        match self {
            Status::Approval => "approval",
            s => s.label(),
        }
    }
    pub fn finished(self) -> bool {
        matches!(self, Status::Done | Status::Failed)
    }
}

/// Whether we still hear from the agent. Independent of what it was doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Conn {
    #[default]
    Connected,
    Stale, // quiet for half its ttl: a warning, not yet an alert
    Lost,  // quiet for a whole ttl
}

impl Conn {
    pub fn label(self) -> &'static str {
        match self {
            Conn::Connected => "connected",
            Conn::Stale => "quiet",
            Conn::Lost => "lost",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Api,     // reported itself over HTTP
    Managed, // launched from the HUD
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Risk {
    Low,
    Medium,
    High,
}

impl Risk {
    pub fn label(self) -> &'static str {
        match self {
            Risk::Low => "low risk",
            Risk::Medium => "medium risk",
            Risk::High => "high risk",
        }
    }
}

/// Heuristic risk of an action, used for styling and to keep allow-rules away from dangerous commands.
pub fn risk(detail: &str) -> Risk {
    const HIGH: [&str; 20] = [
        "rm -rf",
        "rm -fr",
        "rm -r ",
        "sudo ",
        "mkfs",
        "dd if=",
        "--force",
        "push -f",
        "reset --hard",
        "chmod -r 777",
        "drop table",
        "drop database",
        "| sh",
        "| bash",
        "|sh",
        "|bash",
        "shutdown",
        "reboot",
        "> /dev/",
        "truncate ",
    ];
    const MEDIUM: [&str; 16] = [
        "rm ",
        "git push",
        "mv ",
        "chmod",
        "chown",
        "kill",
        "publish",
        "pip install",
        "npm install",
        "apt ",
        "docker ",
        "delete",
        "write:",
        "edit:",
        "curl ",
        "wget ",
    ];
    let d = detail.to_lowercase();
    if HIGH.iter().any(|p| d.contains(p)) {
        Risk::High
    } else if MEDIUM.iter().any(|p| d.contains(p)) {
        Risk::Medium
    } else {
        Risk::Low
    }
}

/// Stable hash of an action (FNV-1a). Lets a client check that the answer it reads is for the action it sent.
pub fn action_hash(s: &str) -> String {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// `*` wildcard match over the whole text.
pub fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || text.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    if !text[first.len()..].ends_with(last) {
        return false;
    }
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    pub latency_ms: u64, // last reported
}

fn now_instant() -> Instant {
    Instant::now()
}

#[derive(Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub task: String,
    pub message: String,
    pub status: Status,
    #[serde(default)]
    pub conn: Conn,
    pub progress: u8,
    #[serde(skip, default = "now_instant")]
    pub seen: Instant,
    pub seen_ts: u64,
    pub started_ts: u64,
    pub ttl_secs: u32,
    pub approval: Option<bool>, // answer to the last approval; agents may poll for it
    pub pid: Option<u32>,
    pub source: Source,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub registered: bool, // id was handed out by POST /register
    #[serde(skip)]
    pub log: VecDeque<String>,
    #[serde(skip)]
    pub last_seq: u64,
    #[serde(skip)]
    pub errors: u32, // consecutive failures
    /// A control the user asked for that the OS has not confirmed yet (e.g. Paused).
    #[serde(skip)]
    pub requested: Option<Status>,
    #[serde(skip)]
    alerted: u8, // bit flags: repeated errors, runaway, budget
}

const ALERT_ERRORS: u8 = 1;
const ALERT_RUNAWAY: u8 = 2;
const ALERT_BUDGET: u8 = 4;

impl Agent {
    pub fn new(name: &str, source: Source) -> Self {
        Agent {
            name: name.into(),
            task: String::new(),
            message: String::new(),
            status: Status::Running,
            conn: Conn::Connected,
            progress: 0,
            seen: Instant::now(),
            seen_ts: now_secs(),
            started_ts: now_secs(),
            ttl_secs: DEFAULT_TTL,
            approval: None,
            pid: None,
            source,
            capabilities: vec![],
            workspace: String::new(),
            usage: Usage::default(),
            registered: false,
            log: VecDeque::new(),
            last_seq: 0,
            errors: 0,
            requested: None,
            alerted: 0,
        }
    }
    pub fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_secs as u64)
    }
    fn touch(&mut self) {
        self.seen = Instant::now();
        self.seen_ts = now_secs();
    }
    /// Needs the user's eyes: failed, or we lost contact.
    pub fn troubled(&self) -> bool {
        self.status == Status::Failed || self.conn == Conn::Lost
    }
}

/// A process we recognised as an AI agent without it reporting anything.
#[derive(Clone, PartialEq)]
pub struct Detected {
    pub pid: u32,
    pub label: String,
    pub cmd: String,
    pub cpu: f32,
    pub mem_mb: u64,
    pub paused: bool,
}

#[derive(Clone, Default, PartialEq)]
pub struct SystemInfo {
    pub cpu: f32, // percent, all cores averaged
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
}

#[derive(Clone, Default, PartialEq)]
pub struct Workspace {
    pub branch: String,
    pub changed: Vec<String>, // from `git status`, capped
    pub git: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
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
    pub options: Vec<String>,        // a question with choices instead of approve / deny
    pub notify: Option<bool>,        // force a native notification for this event
    pub protocol_version: Option<u32>, // envelope: only 1 is understood
    pub event_id: Option<String>,    // envelope: a repeated id is ignored (safe retries)
    pub sequence: Option<u64>, // envelope: per-agent order; older than the last seen is ignored
    pub workspace: Option<String>, // directory the agent works in
    pub capabilities: Option<Vec<String>>,
    pub path: Option<String>, // file.changed
    pub tool: Option<String>, // tool.*
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub cost_usd: Option<f64>,
    pub latency_ms: Option<u64>,
}

/// Why an event or request was refused.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Error {
    UnknownEvent,
    UnsupportedVersion,
    EventIdTooLong,
    BadAgentId,
    NotRegistered,
    TooManyAgents,
    UnknownAgent,
    NotControllable,
    SignalFailed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Error::UnknownEvent => "unknown event type",
            Error::UnsupportedVersion => "unsupported protocol_version",
            Error::EventIdTooLong => "event_id too long",
            Error::BadAgentId => "agent_id must be 1-64 characters of A-Z a-z 0-9 . _ : -",
            Error::NotRegistered => "agent is not registered; POST /api/v1/register first",
            Error::TooManyAgents => "too many agents",
            Error::UnknownAgent => "unknown agent",
            Error::NotControllable => {
                "only agents launched from the HUD can be controlled over the API"
            }
            Error::SignalFailed => "the signal could not be delivered",
        })
    }
}
impl std::error::Error for Error {}

/// A request for the user's decision. Bound to one exact action; never approved by a timeout.
pub struct Approval {
    pub id: String,
    pub agent_id: String,
    pub agent: String,
    pub message: String,
    pub detail: String,
    pub hash: String,
    pub risk: Risk,
    pub options: Vec<String>,
    pub created: Instant,
    pub created_ts: u64,
    pub decided_ts: u64,
    pub ttl: Duration,
    pub answer: Option<bool>,
    pub choice: Option<String>,
    pub expired: bool,
    pub auto: bool, // approved by one of the user's allow rules
}

impl Approval {
    pub fn pending(&self) -> bool {
        self.answer.is_none() && self.choice.is_none() && !self.expired
    }
    pub fn status(&self) -> &'static str {
        match (self.answer, &self.choice, self.expired) {
            (_, Some(_), _) => "answered",
            (Some(true), ..) => "approved",
            (Some(false), ..) => "denied",
            (None, None, true) => "expired",
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
    pub seq: u64, // cursor for GET /history?after=
    #[serde(default)]
    pub agent_id: String,
    pub agent: String,
    pub event: String,
    pub message: String,
}

/// Severity of a history entry, derived from its event name.
pub fn entry_severity(event: &str) -> Severity {
    if event.contains("fail") || event == "approval.required" || event == "approval.denied" {
        Severity::Critical
    } else if [
        "lost",
        "expired",
        "conflict",
        "alert",
        "superseded",
        "stopped",
    ]
    .iter()
    .any(|k| event.contains(k))
    {
        Severity::Warning
    } else {
        Severity::Info
    }
}

/// Something worth telling the user about.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub agent_id: String,
    pub title: String,
    pub body: String,
    pub severity: Severity,
}

/// A notice as kept in the notification history. `shown` is false when preferences silenced it.
#[derive(Serialize, Deserialize, Clone)]
pub struct Note {
    pub ts: u64,
    pub agent_id: String,
    pub title: String,
    pub body: String,
    pub severity: Severity,
    pub shown: bool,
}

#[derive(Clone)]
pub struct Conflict {
    pub ts: u64,
    pub path: String,
    pub first: String,
    pub second: String,
}

#[derive(Default)]
pub struct Store {
    pub cfg: Config,
    pub agents: BTreeMap<String, Agent>,
    pub detected: Vec<Detected>,
    pub scanned: bool, // the first process scan has finished
    pub system: SystemInfo,
    pub workspaces: BTreeMap<String, Workspace>,
    pub history: VecDeque<Entry>,
    pub approvals: Vec<Approval>,
    pub notes: VecDeque<Note>,
    pub conflicts: VecDeque<Conflict>,
    /// Notices waiting to be shown by the OS. The HUD drains this.
    pub outbox: Vec<Notice>,
    /// Problems the user should see (bad config, unwritable data directory).
    pub problems: Vec<String>,
    files: HashMap<String, (String, u64)>, // path -> (agent id, when) for overlap detection
    seen_ids: VecDeque<String>,
    seq: u64,
    dir: Option<PathBuf>,
    dirty: bool,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn load_jsonl<T: DeserializeOwned>(
    file: &Path,
    cap: usize,
    keep: impl Fn(&T) -> bool,
) -> VecDeque<T> {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let tail = &lines[lines.len().saturating_sub(cap)..];
    let kept: Vec<(&str, T)> = tail
        .iter()
        .filter_map(|l| Some((*l, serde_json::from_str::<T>(l).ok()?)))
        .filter(|(_, t)| keep(t))
        .collect();
    if lines.len() > 4 * cap || kept.len() < tail.len() {
        // trim the file: it must not grow without bound, and expired entries leave the disk too
        let body: String = kept.iter().map(|(l, _)| format!("{l}\n")).collect();
        let _ = std::fs::write(file, body);
    }
    kept.into_iter().map(|(_, t)| t).collect()
}

fn append_jsonl<T: Serialize>(dir: &Option<PathBuf>, name: &str, item: &T) -> bool {
    let Some(d) = dir else { return true };
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(d.join(name))
        .and_then(|mut f| writeln!(f, "{}", serde_json::to_string(item).unwrap_or_default()))
        .is_ok()
}

fn clip(s: String, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n - 1).collect::<String>() + "…"
    } else {
        s
    }
}

pub fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c))
}

/// Execution status after an event. Total over every (status, event) pair: unknown combinations
/// keep the current status. Two states stick: a user pause holds until `agent.resumed` /
/// `agent.started` or a terminal event, and an open approval holds until it is answered.
pub fn next_status(old: Status, event: &str, pending_approval: bool) -> Status {
    let new = match event {
        "agent.started" | "agent.resumed" => Status::Running,
        "agent.progress" | "tool.started" | "tool.completed" | "tool.failed" | "file.changed" => {
            if old == Status::Paused {
                Status::Paused
            } else {
                Status::Running
            }
        }
        "agent.waiting" => Status::Waiting,
        "approval.required" => Status::Approval,
        "agent.completed" => Status::Done,
        "agent.failed" | "agent.stopped" => Status::Failed,
        "agent.paused" => Status::Paused,
        _ => old, // heartbeat, resource.updated, checkpoint.created
    };
    if pending_approval && matches!(new, Status::Running | Status::Waiting) {
        Status::Approval
    } else {
        new
    }
}

pub const EVENTS: [&str; 16] = [
    "agent.started",
    "agent.progress",
    "agent.waiting",
    "approval.required",
    "agent.completed",
    "agent.failed",
    "agent.heartbeat",
    "agent.paused",
    "agent.resumed",
    "agent.stopped",
    "tool.started",
    "tool.completed",
    "tool.failed",
    "file.changed",
    "resource.updated",
    "checkpoint.created",
];

impl Store {
    /// Store backed by the data directory: config, history, notifications and the agent list are reloaded.
    pub fn persistent() -> Self {
        let (cfg, err) = Config::load();
        let mut s = Store {
            cfg,
            ..Default::default()
        };
        s.problems.extend(err);
        let Some(dir) = data_dir() else {
            s.problems
                .push("no data directory: nothing will be saved".into());
            return s;
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            s.problems
                .push(format!("cannot create {}: {e}", dir.display()));
            return s;
        }
        let oldest = now_secs().saturating_sub(s.cfg.retention_days.max(1) as u64 * 86_400);
        s.history = load_jsonl(&dir.join("history.jsonl"), HISTORY_CAP, |e: &Entry| {
            e.ts >= oldest
        });
        s.notes = load_jsonl(&dir.join("notifications.jsonl"), NOTES_CAP, |n: &Note| {
            n.ts >= oldest
        });
        s.seq = s.history.iter().map(|e| e.seq).max().unwrap_or(0);
        if let Ok(t) = std::fs::read_to_string(dir.join("agents.json")) {
            match serde_json::from_str::<BTreeMap<String, Agent>>(&t) {
                Ok(agents) => s.agents = agents,
                Err(e) => s
                    .problems
                    .push(format!("agents.json could not be read: {e}")),
            }
        }
        let now = now_secs();
        for a in s.agents.values_mut() {
            // Whatever ran before the restart is not connected now; say so instead of pretending.
            a.seen = Instant::now()
                .checked_sub(Duration::from_secs(now.saturating_sub(a.seen_ts)))
                .unwrap_or_else(Instant::now);
            if a.status.finished() {
                continue;
            }
            if a.status == Status::Approval {
                a.status = Status::Waiting; // open requests do not survive a restart; the agent must ask again
            }
            match a.source {
                Source::Api => a.conn = Conn::Lost,
                Source::Managed => {
                    a.status = Status::Failed;
                    a.message = "the HUD restarted; this process is no longer tracked".into();
                    a.pid = None;
                }
            }
        }
        s.dir = Some(dir);
        s
    }

    /// Write the agent list if it changed. Called by the HUD's ticker and on quit.
    pub fn save_agents(&mut self) {
        if !std::mem::take(&mut self.dirty) {
            return;
        }
        let Some(d) = &self.dir else { return };
        let tmp = d.join("agents.json.tmp");
        let ok = std::fs::write(
            &tmp,
            serde_json::to_string(&self.agents).unwrap_or_default(),
        )
        .and_then(|_| std::fs::rename(&tmp, d.join("agents.json")))
        .is_ok();
        if !ok {
            self.problem("cannot save the agent list");
        }
    }

    pub fn touch(&mut self) {
        self.dirty = true;
    }

    fn problem(&mut self, p: &str) {
        if !self.problems.iter().any(|x| x == p) {
            self.problems.push(p.into());
            crate::log::warn(p, json!({}));
        }
    }

    pub fn record(&mut self, agent_id: &str, agent: &str, event: &str, message: &str) {
        if self
            .history
            .back()
            .is_some_and(|l| l.agent_id == agent_id && l.event == event && l.message == message)
        {
            return; // identical consecutive entry
        }
        self.seq += 1;
        let e = Entry {
            ts: now_secs(),
            seq: self.seq,
            agent_id: agent_id.into(),
            agent: agent.into(),
            event: event.into(),
            message: redact(message), // secrets never reach history or disk
        };
        if !append_jsonl(&self.dir, "history.jsonl", &e) {
            self.problem("cannot write history.jsonl: history will not survive a restart");
        }
        self.history.push_back(e);
        if self.history.len() > HISTORY_CAP {
            self.history.pop_front();
        }
    }

    /// Record a notice and queue it for the OS unless the user's preferences silence it.
    /// Critical notices (approvals, failures) are always shown.
    pub fn notify(&mut self, agent_id: &str, title: String, body: String, severity: Severity) {
        let now = now_secs();
        if self.notes.iter().rev().take(20).any(|n| {
            n.agent_id == agent_id && n.title == title && n.body == body && now - n.ts < 30
        }) {
            return; // same condition, already reported
        }
        let quiet = severity < Severity::Critical
            && (severity < self.cfg.min_severity
                || now < self.cfg.snooze_until
                || self.cfg.muted_agents.iter().any(|m| m == agent_id));
        let body = redact(&body);
        let note = Note {
            ts: now,
            agent_id: agent_id.into(),
            title: title.clone(),
            body: body.clone(),
            severity,
            shown: !quiet,
        };
        append_jsonl(&self.dir, "notifications.jsonl", &note);
        self.notes.push_back(note);
        if self.notes.len() > NOTES_CAP {
            self.notes.pop_front();
        }
        if !quiet {
            self.outbox.push(Notice {
                agent_id: agent_id.into(),
                title,
                body,
                severity,
            });
        }
    }

    fn pending_for(&self, agent_id: &str) -> bool {
        self.approvals
            .iter()
            .any(|x| x.agent_id == agent_id && x.pending())
    }

    /// Explicit registration: the HUD chooses the id. Returns it.
    pub fn register(
        &mut self,
        name: &str,
        capabilities: Vec<String>,
        workspace: &str,
        pid: Option<u32>,
    ) -> Result<String, Error> {
        if self.agents.len() >= AGENT_CAP {
            return Err(Error::TooManyAgents);
        }
        let mut b = [0u8; 6];
        let _ = getrandom::fill(&mut b);
        let id = format!(
            "a-{}",
            b.iter().map(|x| format!("{x:02x}")).collect::<String>()
        );
        let mut a = Agent::new(&clip(name.trim().to_string(), 80), Source::Api);
        if a.name.is_empty() {
            a.name = id.clone();
        }
        a.registered = true;
        a.capabilities = capabilities
            .into_iter()
            .take(16)
            .map(|c| clip(c, 40))
            .collect();
        a.workspace = clip(workspace.into(), 300);
        a.pid = pid;
        a.status = Status::Waiting;
        a.task = "Registered".into();
        let name = a.name.clone();
        self.agents.insert(id.clone(), a);
        self.record(&id, &name, "agent.registered", "");
        self.dirty = true;
        Ok(id)
    }

    /// Apply one event. Err = refused. Notices land in `outbox`.
    pub fn apply(&mut self, e: Event) -> Result<(), Error> {
        if !EVENTS.contains(&e.event.as_str()) {
            return Err(Error::UnknownEvent);
        }
        if e.protocol_version.is_some_and(|v| v != PROTOCOL_VERSION) {
            return Err(Error::UnsupportedVersion);
        }
        if !valid_id(&e.agent_id) {
            return Err(Error::BadAgentId);
        }
        let known = self.agents.get(&e.agent_id);
        let (exists, trusted, last_seq) = (
            known.is_some(),
            known.is_some_and(|a| a.registered || a.source == Source::Managed),
            known.map_or(0, |a| a.last_seq),
        );
        if self.cfg.require_registration && !trusted {
            return Err(Error::NotRegistered);
        }
        if !exists && self.agents.len() >= AGENT_CAP {
            return Err(Error::TooManyAgents);
        }
        if let Some(id) = &e.event_id {
            if id.len() > 128 {
                return Err(Error::EventIdTooLong);
            }
            if self.seen_ids.contains(id) {
                return Ok(()); // retry of an event we already applied
            }
            self.seen_ids.push_back(id.clone());
            if self.seen_ids.len() > SEEN_CAP {
                self.seen_ids.pop_front();
            }
        }
        if e.sequence.is_some_and(|seq| exists && seq <= last_seq) {
            return Ok(()); // arrived out of order: a newer event was already applied
        }
        let kind = e.event.as_str();
        let id = e.agent_id.clone();
        let detail = clip(e.detail.unwrap_or_default(), 4000);
        let approval_id = e.approval_id.clone().unwrap_or_else(|| id.clone());
        let mut auto = false;
        if kind == "approval.required" {
            let hash = action_hash(&detail);
            if let Some(x) = self
                .approvals
                .iter()
                .find(|x| x.id == approval_id && x.pending())
            {
                if x.hash == hash {
                    return Ok(()); // retry of a request that is still open
                }
                // Same id, different action: the old request must not be answered for the new one.
                let (n, d) = (x.agent.clone(), x.detail.clone());
                self.record(&id, &n, "approval.superseded", &d);
            }
            self.approvals.retain(|x| x.id != approval_id);
            auto = e.options.is_empty()
                && risk(&detail) < Risk::High
                && self.cfg.allow_patterns.iter().any(|p| glob(p, &detail));
        }
        let pending = self.pending_for(&id);
        let a = self
            .agents
            .entry(id.clone())
            .or_insert_with(|| Agent::new(&id, Source::Api));
        let (old, was_lost) = (a.status, a.conn == Conn::Lost);
        let new = if auto {
            old
        } else {
            next_status(old, kind, pending)
        };
        a.touch();
        a.conn = Conn::Connected;
        a.requested = None;
        a.last_seq = e.sequence.unwrap_or(a.last_seq);
        if let Some(n) = e.name {
            a.name = clip(n, 80);
        }
        if let Some(t) = e.task {
            a.task = clip(t, 300);
        }
        if let Some(m) = e.message {
            a.message = clip(m, 500);
        }
        if let Some(w) = e.workspace {
            a.workspace = clip(w.trim_end_matches(['/', '\\']).to_string(), 300);
        }
        if let Some(c) = e.capabilities {
            a.capabilities = c.into_iter().take(16).map(|c| clip(c, 40)).collect();
        }
        if a.source == Source::Api {
            a.pid = e.pid.or(a.pid); // a launched agent's pid is set by the HUD only: an event must not redirect its controls
        }
        if let Some(t) = e.ttl {
            a.ttl_secs = t.clamp(5, 86_400);
        }
        if let Some(p) = e.progress {
            a.progress = p.min(100) as u8;
        }
        if kind == "agent.started" {
            a.started_ts = now_secs();
            a.alerted = 0;
            a.errors = 0;
        }
        if new == Status::Done {
            a.progress = 100;
        }
        a.usage.tokens_in += e.tokens_in.unwrap_or(0);
        a.usage.tokens_out += e.tokens_out.unwrap_or(0);
        a.usage.cost_usd += e.cost_usd.unwrap_or(0.0).max(0.0);
        a.usage.latency_ms = e.latency_ms.unwrap_or(a.usage.latency_ms);
        match kind {
            "tool.failed" | "agent.failed" => a.errors += 1,
            "tool.completed" | "agent.completed" | "agent.started" => {
                a.errors = 0;
                a.alerted &= !ALERT_ERRORS;
            }
            _ => {}
        }
        a.status = new;
        let mut alerts: Vec<(String, String)> = vec![];
        if a.errors >= 3 && a.alerted & ALERT_ERRORS == 0 {
            a.alerted |= ALERT_ERRORS;
            alerts.push((
                format!("{} keeps failing", a.name),
                format!("{} errors in a row", a.errors),
            ));
        }
        let budget = self.cfg.budget_usd;
        if budget > 0.0 && a.usage.cost_usd > budget && a.alerted & ALERT_BUDGET == 0 {
            a.alerted |= ALERT_BUDGET;
            alerts.push((
                format!("{} is over budget", a.name),
                format!("${:.2} spent, budget ${budget:.2}", a.usage.cost_usd),
            ));
        }
        let (name, msg, task, ws) = (
            a.name.clone(),
            a.message.clone(),
            a.task.clone(),
            a.workspace.clone(),
        );
        if was_lost {
            self.record(&id, &name, "agent.reconnected", "");
        }
        for (t, b) in alerts {
            self.record(&id, &name, "alert", &t);
            self.notify(&id, t, b, Severity::Warning);
        }
        if kind == "approval.required" {
            self.approvals.push(Approval {
                id: approval_id,
                agent_id: id.clone(),
                agent: name.clone(),
                message: msg.clone(),
                hash: action_hash(&detail),
                risk: risk(&detail),
                detail: detail.clone(),
                options: e.options.into_iter().take(8).map(|o| clip(o, 60)).collect(),
                created: Instant::now(),
                created_ts: now_secs(),
                decided_ts: if auto { now_secs() } else { 0 },
                ttl: Duration::from_secs(
                    e.expires_in.unwrap_or(APPROVAL_TTL).clamp(5, 86_400) as u64
                ),
                answer: auto.then_some(true),
                choice: None,
                expired: false,
                auto,
            });
            if let Some(a) = self.agents.get_mut(&id) {
                a.approval = auto.then_some(true);
            }
        }
        if kind == "file.changed"
            && let Some(p) = e.path.as_deref().filter(|p| !p.is_empty())
        {
            self.file_changed(&id, &name, &ws, p);
        }
        if kind != "agent.heartbeat" && kind != "resource.updated" {
            let tool = e.tool.unwrap_or_default();
            let text = match kind {
                "approval.required" if !detail.is_empty() => detail.clone(),
                "file.changed" => e.path.unwrap_or_default(),
                k if k.starts_with("tool.") => format!("{tool} {msg}").trim().to_string(),
                "agent.progress" | "agent.started" if !task.is_empty() => task,
                _ if !msg.is_empty() => msg.clone(),
                _ => task,
            };
            self.record(&id, &name, if auto { "approval.auto" } else { kind }, &text);
        }
        self.dirty = true;
        let changed = new != old;
        let (what, sev) = match new {
            Status::Approval if kind == "approval.required" => {
                ("needs your decision", Severity::Critical)
            }
            Status::Done if changed => ("finished", Severity::Info),
            Status::Failed if changed => ("failed", Severity::Critical),
            _ if e.notify == Some(true) => ("needs attention", Severity::Warning),
            _ => return Ok(()),
        };
        self.notify(
            &id,
            format!("{name} {what}"),
            if detail.is_empty() { msg } else { detail },
            sev,
        );
        Ok(())
    }

    /// Two agents reporting a change to the same file within a short window is a conflict.
    /// Only agent-reported paths are used, so attribution is never guessed.
    fn file_changed(&mut self, id: &str, name: &str, workspace: &str, path: &str) {
        let full = if Path::new(path).is_absolute() || workspace.is_empty() {
            path.to_string()
        } else {
            format!("{workspace}/{path}")
        };
        let now = now_secs();
        if let Some((other, ts)) = self.files.get(&full).cloned()
            && other != id
            && now - ts < CONFLICT_WINDOW
        {
            let other_name = self
                .agents
                .get(&other)
                .map_or(other.clone(), |a| a.name.clone());
            self.conflicts.push_back(Conflict {
                ts: now,
                path: full.clone(),
                first: other_name.clone(),
                second: name.into(),
            });
            if self.conflicts.len() > 50 {
                self.conflicts.pop_front();
            }
            self.record(
                id,
                name,
                "file.conflict",
                &format!("{full} (also changed by {other_name})"),
            );
            self.notify(
                id,
                format!("{name} and {other_name} changed the same file"),
                full.clone(),
                Severity::Warning,
            );
        }
        if self.files.len() > 2000 {
            self.files.retain(|_, (_, ts)| now - *ts < CONFLICT_WINDOW);
        }
        self.files.insert(full, (id.into(), now));
    }

    /// Heartbeat watchdog (quiet at half the ttl, lost at the ttl; both scaled), approval expiry,
    /// runaway detection and cleanup of long-finished agents.
    pub fn sweep(&mut self, scale: f32) {
        let now = now_secs();
        let (runaway, stale) = (
            self.cfg.runaway_mins as u64 * 60,
            self.cfg.stale_hours as u64 * 3600,
        );
        let mut lost = vec![];
        let mut slow = vec![];
        for (id, a) in self.agents.iter_mut() {
            if a.source == Source::Api && matches!(a.status, Status::Running | Status::Waiting) {
                let quiet = a.seen.elapsed();
                if a.conn != Conn::Lost && quiet > a.ttl().mul_f32(scale) {
                    a.conn = Conn::Lost;
                    lost.push((id.clone(), a.name.clone()));
                } else if a.conn == Conn::Connected && quiet > a.ttl().mul_f32(scale * 0.5) {
                    a.conn = Conn::Stale;
                }
            }
            if runaway > 0
                && a.status == Status::Running
                && a.conn != Conn::Lost
                && now.saturating_sub(a.started_ts) > runaway
                && a.alerted & ALERT_RUNAWAY == 0
            {
                a.alerted |= ALERT_RUNAWAY;
                slow.push((id.clone(), a.name.clone(), (now - a.started_ts) / 60));
            }
        }
        let expired: Vec<(String, String, String)> = self
            .approvals
            .iter_mut()
            .filter(|x| x.pending() && x.created.elapsed() > x.ttl.mul_f32(scale))
            .map(|x| {
                x.expired = true;
                x.decided_ts = now;
                (x.agent_id.clone(), x.agent.clone(), x.detail.clone())
            })
            .collect();
        for (id, name, detail) in expired {
            self.settle(&id);
            self.record(&id, &name, "approval.expired", &detail);
        }
        for (id, name) in lost {
            self.record(&id, &name, "agent.heartbeat_lost", "no heartbeat");
            self.notify(
                &id,
                format!("{name} stopped responding"),
                "heartbeat lost".into(),
                Severity::Warning,
            );
            self.dirty = true;
        }
        for (id, name, mins) in slow {
            self.record(&id, &name, "alert", &format!("running for {mins} minutes"));
            self.notify(
                &id,
                format!("{name} has been running a long time"),
                format!("{mins} minutes"),
                Severity::Warning,
            );
        }
        if stale > 0 {
            let before = self.agents.len();
            let pending: Vec<String> = self
                .pending_approvals()
                .map(|x| x.agent_id.clone())
                .collect();
            self.agents.retain(|id, a| {
                !((a.status.finished() || a.conn == Conn::Lost)
                    && now.saturating_sub(a.seen_ts) > stale
                    && !pending.contains(id))
            });
            self.dirty |= self.agents.len() != before;
        }
    }

    /// After an approval resolves: an agent with nothing else pending goes back to running.
    fn settle(&mut self, agent_id: &str) {
        let pending = self.pending_for(agent_id);
        if let Some(a) = self.agents.get_mut(agent_id) {
            a.touch(); // it was blocked on the user, not silent
            if a.status == Status::Approval && !pending {
                a.status = Status::Running;
            }
        }
        self.dirty = true;
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

    /// Record the user's decision for one request: `Ok(true/false)` for approve / deny, or the chosen option.
    fn decide(&mut self, approval_id: &str, answer: Result<bool, String>) {
        let Some(ap) = self
            .approvals
            .iter_mut()
            .find(|x| x.id == approval_id && x.pending())
        else {
            return; // unknown, already answered or expired: a decision is never replayed or flipped
        };
        let event = match &answer {
            Ok(true) => "approval.granted".to_string(),
            Ok(false) => "approval.denied".to_string(),
            Err(c) if ap.options.contains(c) => format!("question.answered: {c}"),
            Err(_) => return,
        };
        match answer {
            Ok(ok) => ap.answer = Some(ok),
            Err(c) => ap.choice = Some(c),
        }
        ap.decided_ts = now_secs();
        let (id, name, detail, ok) = (
            ap.agent_id.clone(),
            ap.agent.clone(),
            ap.detail.clone(),
            ap.answer,
        );
        if let Some(a) = self.agents.get_mut(&id) {
            a.approval = ok;
        }
        self.settle(&id);
        self.record(&id, &name, &event, &detail);
        crate::log::info(
            "approval decided",
            json!({"approval_id": approval_id, "agent_id": id, "decision": event}),
        );
    }

    pub fn answer(&mut self, approval_id: &str, ok: bool) {
        self.decide(approval_id, Ok(ok));
    }

    pub fn choose(&mut self, approval_id: &str, option: &str) {
        self.decide(approval_id, Err(option.into()));
    }

    pub fn pending_approvals(&self) -> impl Iterator<Item = &Approval> {
        self.approvals.iter().filter(|x| x.pending())
    }

    pub fn push_log(&mut self, id: &str, line: String) {
        if let Some(a) = self.agents.get_mut(id) {
            a.touch();
            a.log.push_back(redact(&line)); // output can echo credentials
            if a.log.len() > LOG_CAP {
                a.log.pop_front();
            }
        }
    }

    /// Agents in an execution status that we are still in contact with, plus detected processes.
    pub fn count(&self, s: Status) -> usize {
        self.agents
            .values()
            .filter(|a| a.status == s && a.conn != Conn::Lost)
            .count()
            + match s {
                Status::Running => self.detected.iter().filter(|d| !d.paused).count(),
                Status::Paused => self.detected.iter().filter(|d| d.paused).count(),
                _ => 0,
            }
    }

    /// Failed agents and agents we lost contact with.
    pub fn troubled(&self) -> usize {
        self.agents.values().filter(|a| a.troubled()).count()
    }

    pub fn agent_json(id: &str, a: &Agent) -> Value {
        json!({
            "agent_id": id, "name": a.name,
            "status": if a.conn == Conn::Lost { "lost" } else { a.status.api() },
            "execution": a.status.api(), "connection": a.conn.label(),
            "task": a.task, "message": a.message, "progress": a.progress, "approval": a.approval,
            "pid": a.pid, "source": a.source, "capabilities": a.capabilities, "workspace": a.workspace,
            "usage": a.usage, "last_seen": a.seen_ts,
        })
    }

    /// Everything needed to investigate a problem, as one JSON document. Credentials are redacted.
    pub fn export(&self) -> Value {
        json!({
            "exported_at": now_secs(),
            "app_version": env!("CARGO_PKG_VERSION"),
            "protocol_version": PROTOCOL_VERSION,
            "agents": self.agents.iter().map(|(id, a)| Store::agent_json(id, a)).collect::<Vec<_>>(),
            "approvals": self.approvals.iter().map(|x| json!({
                "approval_id": x.id, "agent_id": x.agent_id, "detail": redact(&x.detail), "status": x.status(),
                "choice": x.choice, "auto": x.auto, "created": x.created_ts, "decided": x.decided_ts,
            })).collect::<Vec<_>>(),
            "history": self.history,
            "notifications": self.notes,
            "conflicts": self.conflicts.iter().map(|c| json!({"ts": c.ts, "path": c.path, "first": c.first, "second": c.second})).collect::<Vec<_>>(),
            "problems": self.problems,
        })
    }

    /// Write `export()` to `<data dir>/exports/` and return the file.
    pub fn export_to_file(&self) -> Result<PathBuf, String> {
        let dir = self
            .dir
            .clone()
            .or_else(data_dir)
            .ok_or("no data directory")?
            .join("exports");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let file = dir.join(format!("report-{}.json", now_secs()));
        std::fs::write(
            &file,
            serde_json::to_string_pretty(&self.export()).unwrap_or_default(),
        )
        .map_err(|e| e.to_string())?;
        Ok(file)
    }
}

const SECRET_KEYS: [&str; 10] = [
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "access_key",
    "private_key",
    "authorization",
];
const SECRET_PREFIXES: [&str; 9] = [
    "sk-",
    "ghp_",
    "gho_",
    "ghs_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "AIza",
];

fn sensitive_key(k: &str) -> bool {
    let k = k.trim_start_matches('-').to_ascii_lowercase();
    SECRET_KEYS.iter().any(|s| k.contains(s))
}

fn looks_secret(w: &str) -> bool {
    (w.len() >= 16 && SECRET_PREFIXES.iter().any(|p| w.starts_with(p)))
        || (w.len() >= 24 && w.starts_with("eyJ") && w.matches('.').count() == 2) // JWT
}

/// Best-effort removal of credentials from text before it is stored: `key=value` with a
/// secret-looking key, `--token value`, `Bearer x`, well-known token formats and `user:pass@`
/// in URLs. A heuristic, not a guarantee.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut hide_next = false;
    for tok in s.split_inclusive(char::is_whitespace) {
        let word = tok.trim_end();
        let tail = &tok[word.len()..];
        let core = word.trim_matches(|c| "\"'`,;()[]{}".contains(c));
        let mut w = word.to_string();
        if core.is_empty() {
            // nothing to inspect
        } else if hide_next {
            w = w.replace(core, "[redacted]");
            hide_next = false;
        } else if let Some((k, v)) = core.split_once(['=', ':'])
            && !v.is_empty()
            && !v.starts_with("//")
            && (sensitive_key(k) || looks_secret(v))
        {
            w = w.replace(v, "[redacted]");
        } else if looks_secret(core) {
            w = w.replace(core, "[redacted]");
        } else if let Some(i) = core.find("://")
            && let Some(at) = core[i + 3..].find('@')
            && !core[i + 3..i + 3 + at].contains('/')
        {
            let user = &core[i + 3..i + 3 + at];
            w = w.replace(user, "[redacted]");
        } else if core.eq_ignore_ascii_case("bearer")
            || (core.starts_with('-') && sensitive_key(core) && !core.contains('='))
        {
            hide_next = true;
        }
        out.push_str(&w);
        out.push_str(tail);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn ev(kind: &str, id: &str) -> Event {
        Event {
            event: kind.into(),
            agent_id: id.into(),
            ..Default::default()
        }
    }

    fn ask(id: &str, aid: &str, detail: &str) -> Event {
        Event {
            approval_id: Some(aid.into()),
            detail: Some(detail.into()),
            ..ev("approval.required", id)
        }
    }

    fn notices(s: &mut Store) -> usize {
        std::mem::take(&mut s.outbox).len()
    }

    #[test]
    fn lifecycle() {
        let mut s = Store::default();
        s.apply(ev("agent.started", "a")).unwrap();
        assert_eq!(notices(&mut s), 0); // no noise on start
        s.apply(ask("a", "r1", "rm -rf build")).unwrap();
        assert_eq!(s.outbox[0].severity, Severity::Critical);
        assert_eq!(notices(&mut s), 1);
        s.apply(ask("a", "r1", "rm -rf build")).unwrap(); // retry of open request: no spam, no duplicate
        assert_eq!((notices(&mut s), s.approvals.len()), (0, 1));
        assert_eq!(s.agents["a"].status, Status::Approval);
        s.answer("r1", true);
        assert_eq!(
            (s.agents["a"].status, s.agents["a"].approval),
            (Status::Running, Some(true))
        );
        s.answer("r1", false); // an answered approval can't be flipped or replayed
        assert_eq!(s.approvals[0].status(), "approved");
        assert!(s.approvals[0].decided_ts > 0); // every decision is timestamped
        s.apply(ev("agent.completed", "a")).unwrap();
        assert_eq!((notices(&mut s), s.agents["a"].progress), (1, 100));
        assert_eq!(s.apply(ev("bogus", "a")), Err(Error::UnknownEvent));
    }

    #[test]
    fn approvals_are_independent_and_never_approved_by_timeout() {
        let mut s = Store::default();
        s.apply(ask("a", "r1", "git push")).unwrap();
        s.apply(ask("a", "r2", "rm -rf /tmp/x")).unwrap();
        s.apply(ev("agent.progress", "a")).unwrap(); // progress must not hide an open approval
        assert_eq!(s.agents["a"].status, Status::Approval);
        s.answer("r1", false);
        assert_eq!(s.agents["a"].status, Status::Approval); // r2 still open
        assert_eq!(
            (s.approvals[0].status(), s.approvals[1].status()),
            ("denied", "pending")
        );
        s.sweep(0.0); // expire everything
        assert_eq!(s.approvals[1].status(), "expired"); // timeout means expired, never approved
        assert_eq!(
            (s.agents["a"].status, s.agents["a"].conn),
            (Status::Running, Conn::Connected)
        ); // waiting on the user must not read as "lost"
    }

    #[test]
    fn a_changed_action_needs_a_new_decision() {
        let mut s = Store::default();
        s.apply(ask("a", "r1", "git push origin dev")).unwrap();
        let h = s.approvals[0].hash.clone();
        s.apply(ask("a", "r1", "git push --force origin main"))
            .unwrap(); // same id, different action
        assert_eq!(s.approvals.len(), 1);
        assert_ne!(s.approvals[0].hash, h);
        assert_eq!(
            (s.approvals[0].risk, s.approvals[0].status()),
            (Risk::High, "pending")
        );
        assert!(s.history.iter().any(|e| e.event == "approval.superseded"));
        assert_eq!(
            (risk("ls -la"), risk("git push"), risk("sudo rm x")),
            (Risk::Low, Risk::Medium, Risk::High)
        );
    }

    #[test]
    fn allow_rules_skip_the_prompt_but_never_for_high_risk() {
        let mut s = Store::default();
        s.cfg.allow_patterns = vec!["$ cargo *".into(), "$ rm -rf *".into()];
        s.apply(ask("a", "r1", "$ cargo test --release")).unwrap();
        assert_eq!(
            (s.approvals[0].status(), s.approvals[0].auto),
            ("approved", true)
        );
        assert_eq!((notices(&mut s), s.agents["a"].approval), (0, Some(true)));
        assert!(s.history.iter().any(|e| e.event == "approval.auto")); // still on the record
        s.apply(ask("a", "r2", "$ rm -rf /")).unwrap();
        assert_eq!(s.approvals[1].status(), "pending"); // a rule can't wave through a dangerous command
        assert!(
            glob("a*c*e", "abcde") && glob("*", "x") && !glob("a*c", "abd") && !glob("ab", "abc")
        );
    }

    #[test]
    fn questions_take_one_of_the_offered_choices() {
        let mut s = Store::default();
        let mut q = ask("a", "q1", "Which database?");
        q.options = vec!["postgres".into(), "sqlite".into()];
        s.apply(q).unwrap();
        s.choose("q1", "mysql"); // not offered
        assert_eq!(s.approvals[0].status(), "pending");
        s.choose("q1", "sqlite");
        assert_eq!(
            (s.approvals[0].status(), s.approvals[0].choice.as_deref()),
            ("answered", Some("sqlite"))
        );
        assert_eq!(s.agents["a"].status, Status::Running);
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
        assert!(s.history[1].seq > s.history[0].seq);
    }

    #[test]
    fn heartbeat_quiet_then_lost_then_recovered() {
        let mut s = Store::default();
        s.apply(ev("agent.started", "a")).unwrap();
        s.agents.get_mut("a").unwrap().seen = Instant::now() - Duration::from_secs(20); // ttl 30
        s.sweep(1.0);
        assert_eq!((s.agents["a"].conn, notices(&mut s)), (Conn::Stale, 0)); // warning stage: no alert yet
        s.sweep(0.0);
        assert_eq!(
            (s.agents["a"].conn, s.agents["a"].status),
            (Conn::Lost, Status::Running)
        ); // what it was doing is kept
        assert_eq!(notices(&mut s), 1);
        s.sweep(0.0);
        assert_eq!(notices(&mut s), 0); // notified once only
        assert_eq!(s.history.back().unwrap().agent_id, "a"); // filed under the agent's id
        s.apply(ev("agent.heartbeat", "a")).unwrap();
        assert_eq!(s.agents["a"].conn, Conn::Connected);
        assert_eq!(s.history.back().unwrap().event, "agent.reconnected");
    }

    #[test]
    fn every_transition_is_defined() {
        for old in Status::ALL {
            for kind in EVENTS {
                for pending in [false, true] {
                    let new = next_status(old, kind, pending);
                    if pending {
                        assert!(
                            !matches!(new, Status::Running | Status::Waiting),
                            "{old:?} {kind}"
                        ); // an open approval is never hidden
                    }
                    if matches!(
                        kind,
                        "agent.heartbeat" | "resource.updated" | "checkpoint.created"
                    ) && !pending
                    {
                        assert_eq!(new, old); // these never change what the agent is doing
                    }
                }
            }
            assert_eq!(next_status(old, "agent.completed", false), Status::Done);
            assert_eq!(next_status(old, "agent.failed", false), Status::Failed);
        }
        assert_eq!(
            next_status(Status::Paused, "agent.progress", false),
            Status::Paused
        ); // a pause sticks
        assert_eq!(
            next_status(Status::Paused, "agent.resumed", false),
            Status::Running
        );
        assert_eq!(
            next_status(Status::Done, "agent.progress", false),
            Status::Running
        ); // picked up again
        let mut s = Store::default();
        let mut e = ev("agent.started", "a");
        e.ttl = Some(900);
        s.apply(e).unwrap();
        assert_eq!(s.agents["a"].ttl(), Duration::from_secs(900));
    }

    #[test]
    fn envelope_version_dedup_and_sequence() {
        let mut s = Store::default();
        let mut e = ev("agent.started", "a");
        e.protocol_version = Some(2);
        assert_eq!(s.apply(e), Err(Error::UnsupportedVersion));
        let step = |n: u64, id: &str, msg: &str| Event {
            event_id: Some(id.into()),
            sequence: Some(n),
            message: Some(msg.into()),
            protocol_version: Some(1),
            ..ev("agent.progress", "a")
        };
        s.apply(step(2, "e2", "step 2")).unwrap();
        s.apply(step(2, "e2", "retry, must be ignored")).unwrap();
        s.apply(step(1, "e1", "late, must be ignored")).unwrap();
        assert_eq!(s.agents["a"].message, "step 2");
        s.apply(step(3, "e3", "step 3")).unwrap();
        assert_eq!(s.agents["a"].message, "step 3");
        let mut long = ev("agent.progress", "a");
        long.event_id = Some("x".repeat(200));
        assert_eq!(s.apply(long), Err(Error::EventIdTooLong));
        for bad in ["", "../etc/passwd", "a b", &"x".repeat(65)] {
            assert_eq!(
                s.apply(ev("agent.started", bad)),
                Err(Error::BadAgentId),
                "{bad}"
            );
        }
    }

    #[test]
    fn an_event_cannot_redirect_a_launched_agents_pid() {
        let mut s = Store::default();
        let mut m = Agent::new("launched", Source::Managed);
        m.pid = Some(4242);
        s.agents.insert("managed-1".into(), m);
        let hostile = |pid| Event {
            pid: Some(pid),
            ..ev("agent.progress", "managed-1")
        };
        s.apply(hostile(1)).unwrap();
        assert_eq!(s.agents["managed-1"].pid, Some(4242)); // still the process the HUD started
        s.agents.get_mut("managed-1").unwrap().pid = None; // it exited
        s.apply(hostile(1)).unwrap();
        assert_eq!(s.agents["managed-1"].pid, None); // and cannot be re-armed with another pid
        let mut e = ev("agent.started", "mine");
        e.pid = Some(77);
        s.apply(e).unwrap();
        assert_eq!(s.agents["mine"].pid, Some(77)); // an agent may still report its own
    }

    #[test]
    fn registration_assigns_the_id() {
        let mut s = Store::default();
        s.cfg.require_registration = true;
        assert_eq!(
            s.apply(ev("agent.started", "i-made-this-up")),
            Err(Error::NotRegistered)
        );
        let id = s
            .register("Builder", vec!["rust".into()], "/work/proj", None)
            .unwrap();
        assert!(valid_id(&id) && s.agents[&id].capabilities == ["rust"]);
        s.apply(ev("agent.started", &id)).unwrap();
        assert_eq!(s.agents[&id].status, Status::Running);
    }

    #[test]
    fn tools_files_usage_and_alerts() {
        let mut s = Store::default();
        s.cfg.budget_usd = 1.0;
        let file = |id: &str, p: &str| Event {
            path: Some(p.into()),
            workspace: Some("/w".into()),
            ..ev("file.changed", id)
        };
        s.apply(file("a", "src/main.rs")).unwrap();
        s.apply(file("a", "src/main.rs")).unwrap(); // same agent twice is not a conflict
        assert!(s.conflicts.is_empty());
        s.apply(file("b", "/w/src/main.rs")).unwrap(); // relative and absolute forms meet
        assert_eq!((s.conflicts.len(), notices(&mut s)), (1, 1));
        for _ in 0..3 {
            s.apply(Event {
                tool: Some("Bash".into()),
                ..ev("tool.failed", "a")
            })
            .unwrap();
        }
        assert_eq!(
            (s.agents["a"].status, notices(&mut s)),
            (Status::Running, 1)
        ); // repeated errors: one alert
        s.apply(Event {
            tokens_in: Some(10),
            tokens_out: Some(5),
            cost_usd: Some(0.6),
            ..ev("resource.updated", "a")
        })
        .unwrap();
        s.apply(Event {
            cost_usd: Some(0.6),
            ..ev("resource.updated", "a")
        })
        .unwrap();
        let u = s.agents["a"].usage;
        assert_eq!((u.tokens_in, u.tokens_out, notices(&mut s)), (10, 5, 1)); // over budget: one alert
        assert!(s.outbox.is_empty());
    }

    #[test]
    fn preferences_silence_everything_but_critical() {
        let mut s = Store::default();
        s.cfg.muted_agents = vec!["a".into()];
        s.apply(ev("agent.started", "a")).unwrap();
        s.apply(ev("agent.completed", "a")).unwrap();
        assert_eq!(
            (notices(&mut s), s.notes.len(), s.notes[0].shown),
            (0, 1, false)
        ); // kept in history, not shown
        s.apply(ev("agent.failed", "a")).unwrap();
        assert_eq!(notices(&mut s), 1); // critical always gets through
        s.cfg.muted_agents.clear();
        s.cfg.snooze_until = now_secs() + 60;
        s.apply(ev("agent.started", "b")).unwrap();
        s.apply(ev("agent.completed", "b")).unwrap();
        assert_eq!(notices(&mut s), 0);
    }

    #[test]
    fn stale_agents_are_cleaned_up_and_board_is_bounded() {
        let mut s = Store::default();
        s.apply(ev("agent.completed", "old")).unwrap();
        s.apply(ev("agent.started", "live")).unwrap();
        s.agents.get_mut("old").unwrap().seen_ts -= 25 * 3600;
        s.agents.get_mut("live").unwrap().seen_ts -= 25 * 3600; // still running: kept
        s.sweep(1000.0);
        assert_eq!(s.agents.keys().collect::<Vec<_>>(), ["live"]);
        for i in 0..AGENT_CAP {
            let _ = s.apply(ev("agent.started", &format!("n{i}")));
        }
        assert_eq!(
            s.apply(ev("agent.started", "one-too-many")),
            Err(Error::TooManyAgents)
        );
    }

    #[test]
    fn secrets_are_redacted_from_history_and_output_not_from_the_approval() {
        let r = redact("curl -H 'Authorization: Bearer abcdef123456' https://u:pw@host/x");
        assert!(!r.contains("abcdef123456") && !r.contains("u:pw"), "{r}");
        assert!(redact("GITHUB_TOKEN=ghp_aaaaaaaaaaaaaaaaaaaa npm publish").contains("[redacted]"));
        assert!(!redact("deploy --password hunter2 now").contains("hunter2"));
        assert!(!redact("export KEY=sk-abcdefghijklmnopqrstu").contains("sk-abc"));
        assert_eq!(redact("cargo build --release"), "cargo build --release"); // ordinary text untouched
        assert_eq!(redact("echo author:bob"), "echo author:bob");
        let mut s = Store::default();
        s.apply(ask("a", "r1", "psql postgres://admin:s3cret@db/prod"))
            .unwrap();
        assert!(s.approvals[0].detail.contains("s3cret")); // user must see the exact action
        assert!(!s.history.back().unwrap().message.contains("s3cret")); // but history is scrubbed
        assert!(!s.outbox[0].body.contains("s3cret")); // and so is the notification
        s.push_log("a", "login --password hunter2".into());
        assert!(!s.agents["a"].log[0].contains("hunter2"));
        assert!(!s.export().to_string().contains("s3cret"));
    }

    #[test]
    fn agents_history_and_notifications_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("acc-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = Store {
            dir: Some(dir.clone()),
            ..Default::default()
        };
        s.apply(ev("agent.started", "a")).unwrap();
        s.apply(ev("agent.completed", "b")).unwrap();
        s.save_agents();
        let agents: BTreeMap<String, Agent> =
            serde_json::from_str(&std::fs::read_to_string(dir.join("agents.json")).unwrap())
                .unwrap();
        assert_eq!(
            (agents["a"].status, agents["b"].status),
            (Status::Running, Status::Done)
        );
        let h: VecDeque<Entry> = load_jsonl(&dir.join("history.jsonl"), 10, |_| true);
        let n: VecDeque<Note> = load_jsonl(&dir.join("notifications.jsonl"), 10, |_| true);
        assert_eq!((h.len(), n.len()), (2, 1));
        let none: VecDeque<Entry> = load_jsonl(&dir.join("history.jsonl"), 10, |_| false); // retention drops old entries from disk
        assert!(
            none.is_empty()
                && std::fs::read_to_string(dir.join("history.jsonl"))
                    .unwrap()
                    .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
