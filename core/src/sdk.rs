//! Client SDK: report an agent's progress to the HUD and ask the user for approval.
//!
//! ```no_run
//! use acc_core::sdk::Client;
//! use serde_json::json;
//! let agent = Client::register("Nightly research", &["search"]).expect("HUD not running");
//! agent.emit("agent.started", json!({"task": "Collect sources"}));
//! if agent.approve("rm -rf ./cache", 120) {
//!     // the user said yes
//! }
//! agent.emit("agent.completed", json!({"message": "done"}));
//! ```
//! Every call returns quickly and never panics when the HUD is not running.
use crate::cli::{self, Verdict};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Client {
    pub agent_id: String,
    seq: AtomicU64,
}

impl Client {
    /// Use an id you already have (for example `$ACC_AGENT_ID` in a process launched from the HUD).
    pub fn with_id(agent_id: &str) -> Self {
        Client {
            agent_id: agent_id.into(),
            seq: AtomicU64::new(0),
        }
    }

    /// Ask the HUD for an id. None when it is not running.
    pub fn register(name: &str, capabilities: &[&str]) -> Option<Self> {
        let body = json!({"name": name, "capabilities": capabilities, "pid": std::process::id(),
            "workspace": std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default()});
        let (code, text) = cli::request("POST", "/register", &body.to_string())?;
        let v: Value = serde_json::from_str(&text).ok()?;
        (code == 200).then(|| Client::with_id(v["agent_id"].as_str().unwrap_or_default()))
    }

    /// Send one event; `fields` are merged in (task, message, progress, path, tokens_in…). True if delivered.
    pub fn emit(&self, event: &str, fields: Value) -> bool {
        let mut e = if fields.is_object() {
            fields
        } else {
            json!({})
        };
        e["event"] = event.into();
        e["agent_id"] = self.agent_id.as_str().into();
        e["protocol_version"] = crate::store::PROTOCOL_VERSION.into();
        e["sequence"] = (self.seq.fetch_add(1, Ordering::SeqCst) + 1).into();
        cli::post(&e)
    }

    pub fn heartbeat(&self) -> bool {
        self.emit("agent.heartbeat", json!({}))
    }

    /// True only if the user approved this exact action in the HUD before `timeout_secs` ran out.
    pub fn approve(&self, action: &str, timeout_secs: u32) -> bool {
        cli::ask(&self.agent_id, None, action, timeout_secs) == Verdict::Approved
    }

    /// Ask a question with choices. None = no answer.
    pub fn choose(&self, question: &str, options: &[String], timeout_secs: u32) -> Option<String> {
        cli::ask_with(&self.agent_id, None, question, options, timeout_secs)
    }
}
