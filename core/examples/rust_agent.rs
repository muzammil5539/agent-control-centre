//! Minimal Rust agent using the SDK client. Run the HUD first, then:
//!   cargo run -p acc-core --example rust_agent
use acc_core::sdk::Client;
use serde_json::json;
use std::{thread::sleep, time::Duration};

fn main() {
    let Some(agent) = Client::register("Rust demo", &["demo"]) else {
        eprintln!("cannot reach the HUD (is it running?)");
        std::process::exit(1);
    };
    agent.emit(
        "agent.started",
        json!({"task": "Rotate the logs", "ttl": 30}),
    );
    for progress in [30, 70] {
        sleep(Duration::from_millis(700));
        agent.emit("agent.progress", json!({"progress": progress}));
    }
    if agent.approve("$ rm -rf logs/old", 120) {
        agent.emit("agent.completed", json!({"message": "old logs removed"}));
    } else {
        agent.emit(
            "agent.failed",
            json!({"message": "not approved, nothing was deleted"}),
        );
    }
}
