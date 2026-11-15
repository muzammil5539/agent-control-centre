//! User configuration: `config.json` in the data directory. Every field is optional; a broken
//! file falls back to defaults and the reason is shown in the HUD. `ACC_ADDR`, `ACC_HOTKEY`
//! and `ACC_DATA_DIR` override it (used by tests and second instances).
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::PathBuf, sync::OnceLock};

pub const DEFAULT_ADDR: &str = "127.0.0.1:7777";
pub const DEFAULT_HOTKEY: &str = "ctrl+alt+KeyA";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Listen address. Must be a loopback address; anything else is refused.
    pub addr: String,
    pub hotkey: String,
    /// Default Claude Code gate for `acc hook`: off | bash | mutating | all.
    pub gate: String,
    pub gate_timeout: u32,
    pub start_hidden: bool,
    pub compact: bool,
    /// Window opacity, 0.3 to 1.0 (needs a compositing desktop).
    pub opacity: f32,
    /// OS notifications below this severity are recorded but not shown.
    pub min_severity: Severity,
    /// Agent ids whose non-critical notifications are not shown.
    pub muted_agents: Vec<String>,
    /// Unix time until which non-critical notifications are not shown.
    pub snooze_until: u64,
    pub retention_days: u32,
    /// Finished or lost agents are removed from the board after this many hours.
    pub stale_hours: u32,
    /// Alert when an agent process uses more CPU (%) or memory (MB) than this. 0 = off.
    pub cpu_alert: f32,
    pub mem_alert_mb: u64,
    /// Alert when an agent has been running longer than this many minutes. 0 = off.
    pub runaway_mins: u32,
    /// Alert when an agent reports more spend than this (USD). 0 = off.
    pub budget_usd: f64,
    /// Actions matching one of these (`*` wildcard) are approved without asking. Never applies to high-risk actions.
    pub allow_patterns: Vec<String>,
    /// Only accept events from ids handed out by POST /register.
    pub require_registration: bool,
    /// Queued launches start while fewer than this many managed agents run.
    pub max_parallel: usize,
    /// Requests per second accepted by the local API.
    pub rate_limit: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            addr: DEFAULT_ADDR.into(),
            hotkey: DEFAULT_HOTKEY.into(),
            gate: "off".into(),
            gate_timeout: 120,
            start_hidden: false,
            compact: false,
            opacity: 1.0,
            min_severity: Severity::Info,
            muted_agents: vec![],
            snooze_until: 0,
            retention_days: 30,
            stale_hours: 24,
            cpu_alert: 0.0,
            mem_alert_mb: 0,
            runaway_mins: 0,
            budget_usd: 0.0,
            allow_patterns: vec![],
            require_registration: false,
            max_parallel: 2,
            rate_limit: 300,
        }
    }
}

pub fn data_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("ACC_DATA_DIR") {
        return Some(d.into());
    }
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA")?.into()
    } else if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(x)
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".local/share")
    };
    Some(base.join("agent-control-centre"))
}

pub fn path() -> Option<PathBuf> {
    Some(data_dir()?.join("config.json"))
}

/// Parse config text. Err carries the reason; callers fall back to defaults.
pub fn parse(text: &str) -> Result<Config, String> {
    let mut c: Config = serde_json::from_str(text).map_err(|e| format!("config.json: {e}"))?;
    c.opacity = c.opacity.clamp(0.3, 1.0);
    if !c
        .addr
        .parse::<SocketAddr>()
        .is_ok_and(|a| a.ip().is_loopback())
    {
        let bad = std::mem::replace(&mut c.addr, DEFAULT_ADDR.into());
        return Err(format!(
            "config.json: addr '{bad}' is not a loopback address; using {DEFAULT_ADDR}"
        ));
    }
    Ok(c)
}

impl Config {
    /// The saved configuration (defaults when there is none) and any problem found reading it.
    pub fn load() -> (Config, Option<String>) {
        let (mut c, mut err) = match path().and_then(|p| std::fs::read_to_string(p).ok()) {
            None => (Config::default(), None),
            Some(t) => match parse(&t) {
                Ok(c) => (c, None),
                Err(e) => (Config::default(), Some(e)),
            },
        };
        if let Ok(a) = std::env::var("ACC_ADDR") {
            if a.parse::<SocketAddr>().is_ok_and(|a| a.ip().is_loopback()) {
                c.addr = a;
            } else {
                err = Some(format!("ACC_ADDR '{a}' is not a loopback address; ignored"));
            }
        }
        if let Ok(h) = std::env::var("ACC_HOTKEY") {
            c.hotkey = h;
        }
        (c, err)
    }

    pub fn save(&self) -> Result<(), String> {
        let p = path().ok_or("no data directory")?;
        let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
        std::fs::write(&p, serde_json::to_string_pretty(self).unwrap_or_default())
            .map_err(|e| format!("cannot save {}: {e}", p.display()))
    }
}

/// Address clients talk to (resolved once per process).
pub fn addr() -> &'static str {
    static A: OnceLock<String> = OnceLock::new();
    A.get_or_init(|| Config::load().0.addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_partial_files_and_loopback_only() {
        assert_eq!(parse("{}").unwrap(), Config::default());
        let c = parse(r#"{"addr":"127.0.0.1:9000","opacity":5,"muted_agents":["a"]}"#).unwrap();
        assert_eq!(
            (c.addr.as_str(), c.opacity, c.gate_timeout),
            ("127.0.0.1:9000", 1.0, 120)
        );
        assert!(parse(r#"{"addr":"0.0.0.0:7777"}"#).is_err()); // never listen beyond this machine
        assert!(parse("not json").is_err());
        let back = parse(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c); // round trip
    }
}
