//! End to end over real sockets and real processes: three independent agents (separate `acc`
//! processes) talk to the API; one is approved, one denied, one goes silent and comes back;
//! then the HUD "restarts" and history and agents are still there.
#![cfg(unix)] // drives sh, sleep and signals
use acc_core::store::{Conn, Status, Store};
use acc_core::{api, auth};
use std::{
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let t = Instant::now();
    while !ok() {
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn three_agents_approval_disconnect_restart() {
    let dir = std::env::temp_dir().join(format!("acc-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr = format!("127.0.0.1:{port}");
    // SAFETY: the only test in this binary; set before anything reads the environment.
    unsafe {
        std::env::set_var("ACC_DATA_DIR", &dir);
        std::env::set_var("ACC_ADDR", &addr);
    }
    let store = Arc::new(Mutex::new(Store::persistent()));
    let server = tiny_http::Server::http(addr.as_str()).unwrap();
    let token = auth::load_or_create();
    std::thread::spawn({
        let s = store.clone();
        move || api::serve(server, s, api::Api::new(token), || {})
    });
    let acc = |args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_acc"));
        c.args(args)
            .env("ACC_DATA_DIR", &dir)
            .env("ACC_ADDR", &addr)
            .stdin(Stdio::null());
        c
    };
    let locked = || store.lock().unwrap();

    // Agent A: an ordinary command wrapped by `acc run`. It registers, stays alive, and finishes.
    let mut a = acc(&["run", "--name", "Agent A", "--", "sh", "-c", "sleep 1"])
        .spawn()
        .unwrap();
    // Agent B: gets its id from the HUD, starts with a short heartbeat ttl, then goes quiet.
    let out = acc(&["register", "Agent B", "--capabilities", "search"])
        .output()
        .unwrap();
    let b = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert!(out.status.success() && b.starts_with("a-"), "register: {b}");
    assert!(
        acc(&["emit", "agent.started", &b, "--task", "crawl", "--ttl", "5"])
            .status()
            .unwrap()
            .success()
    );
    // Agent C: asks before a destructive action and blocks on the answer. A second request is denied.
    let mut c1 = acc(&[
        "ask",
        "agent-c",
        "--detail",
        "rm -rf build",
        "--timeout",
        "30",
    ])
    .spawn()
    .unwrap();
    wait_for("the first approval request", || {
        locked().pending_approvals().count() == 1
    });
    let mut c2 = acc(&[
        "ask",
        "agent-c",
        "--detail",
        "git push --force",
        "--timeout",
        "30",
    ])
    .spawn()
    .unwrap();
    wait_for("both approval requests", || {
        locked().pending_approvals().count() == 2
    });

    {
        let s = locked();
        assert_eq!(s.agents.len(), 3);
        assert_eq!(s.agents["agent-c"].status, Status::Approval);
        assert!(
            s.agents
                .values()
                .any(|x| x.name == "Agent A" && x.status == Status::Running && x.pid.is_some())
        );
    }
    // An unauthenticated client gets nothing.
    let code = Command::new(env!("CARGO_BIN_EXE_acc"))
        .args(["status"])
        .env("ACC_ADDR", &addr)
        .env("ACC_DATA_DIR", dir.join("someone-else"))
        .output()
        .unwrap();
    assert!(!code.status.success());

    // The user decides in the HUD (the only place a decision can come from).
    let ids: Vec<(String, String)> = locked()
        .pending_approvals()
        .map(|x| (x.id.clone(), x.detail.clone()))
        .collect();
    for (id, detail) in &ids {
        locked().answer(id, detail == "rm -rf build");
    }
    assert_eq!(c1.wait().unwrap().code(), Some(0)); // approved
    assert_eq!(c2.wait().unwrap().code(), Some(1)); // denied
    assert_eq!(locked().agents["agent-c"].status, Status::Running);

    // A finishes on its own.
    assert!(a.wait().unwrap().success());
    wait_for("agent A to report completion", || {
        locked()
            .agents
            .values()
            .any(|x| x.name == "Agent A" && x.status == Status::Done)
    });

    // B says nothing for longer than its ttl: quiet, then lost, with one notification.
    wait_for("agent B to be flagged lost", || {
        let mut s = locked();
        s.sweep(1.0);
        s.agents[&b].conn == Conn::Lost
    });
    assert!(
        locked()
            .notes
            .iter()
            .any(|n| n.title.contains("Agent B stopped responding"))
    );
    assert!(
        acc(&["emit", "agent.heartbeat", &b])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(locked().agents[&b].conn, Conn::Connected); // reconnected

    // Restart: a new store from the same directory has the history, the notifications and the agents.
    let (history, notes) = {
        let mut s = locked();
        s.save_agents();
        (s.history.len(), s.notes.len())
    };
    let again = Store::persistent();
    assert!(history >= 8, "history: {history}");
    assert_eq!(
        (again.history.len(), again.notes.len(), again.agents.len()),
        (history, notes, 3)
    );
    assert!(
        again
            .history
            .iter()
            .any(|e| e.event == "approval.denied" && e.message == "git push --force")
    );
    assert_eq!(again.agents[&b].conn, Conn::Lost); // not connected until it reports again
    assert!(
        again
            .agents
            .values()
            .any(|x| x.name == "Agent A" && x.status == Status::Done)
    );
    assert!(again.problems.is_empty(), "{:?}", again.problems);
    let _ = std::fs::remove_dir_all(&dir);
}
