//! Agent Control Centre: a floating always-on-top HUD that shows every AI agent
//! on this machine. Agents report over a localhost HTTP event API (see README).
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use eframe::egui::{self, Color32, RichText};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::BTreeMap,
    io::Read,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const ADDR: &str = "127.0.0.1:7777"; // localhost only: any local process may report
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Status {
    Running,
    Waiting,
    Approval,
    Done,
    Failed,
    Lost,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Waiting => "waiting",
            Status::Approval => "approval",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Lost => "lost",
        }
    }
    fn color(self) -> Color32 {
        match self {
            Status::Running => Color32::from_rgb(80, 200, 120),
            Status::Waiting => Color32::from_rgb(240, 190, 70),
            Status::Approval => Color32::from_rgb(255, 140, 60),
            Status::Done => Color32::from_rgb(110, 160, 230),
            Status::Failed => Color32::from_rgb(235, 80, 80),
            Status::Lost => Color32::from_rgb(160, 120, 200),
        }
    }
}

struct Agent {
    name: String,
    task: String,
    message: String,
    status: Status,
    progress: u8,
    seen: Instant,
    approval: Option<bool>, // answer to the last approval.required; agents poll for it
}

#[derive(Deserialize)]
struct Event {
    event: String,
    agent_id: String,
    name: Option<String>,
    task: Option<String>,
    message: Option<String>,
    progress: Option<u32>,
}

#[derive(Default)]
struct Store {
    agents: BTreeMap<String, Agent>,
}

type Notice = (String, String); // (title, body) for a native notification

impl Store {
    /// Apply one event. Err = unknown event type. Ok(Some) = user should be notified.
    fn apply(&mut self, e: Event) -> Result<Option<Notice>, ()> {
        let a = self
            .agents
            .entry(e.agent_id.clone())
            .or_insert_with(|| Agent {
                name: e.agent_id.clone(),
                task: String::new(),
                message: String::new(),
                status: Status::Running,
                progress: 0,
                seen: Instant::now(),
                approval: None,
            });
        let old = a.status;
        let new = match e.event.as_str() {
            "agent.started" | "agent.progress" => Status::Running,
            "agent.waiting" => Status::Waiting,
            "approval.required" => Status::Approval,
            "agent.completed" => Status::Done,
            "agent.failed" => Status::Failed,
            "agent.heartbeat" if old == Status::Lost => Status::Running,
            "agent.heartbeat" => old,
            _ => return Err(()),
        };
        a.seen = Instant::now();
        a.name = e.name.unwrap_or_else(|| a.name.clone());
        a.task = e.task.unwrap_or_else(|| a.task.clone());
        a.message = e.message.unwrap_or_else(|| a.message.clone());
        if let Some(p) = e.progress {
            a.progress = p.min(100) as u8;
        }
        if new == Status::Done {
            a.progress = 100;
        }
        if new == Status::Approval {
            a.approval = None;
        }
        a.status = new;
        let notify = new != old && matches!(new, Status::Approval | Status::Done | Status::Failed);
        Ok(notify.then(|| {
            let what = match new {
                Status::Approval => "needs your approval",
                Status::Done => "finished",
                _ => "failed",
            };
            (format!("{} {what}", a.name), a.message.clone())
        }))
    }

    /// Mark silent running/waiting agents as lost; returns notices for the newly lost.
    fn sweep(&mut self, timeout: Duration) -> Vec<Notice> {
        let mut out = vec![];
        for a in self.agents.values_mut() {
            if matches!(a.status, Status::Running | Status::Waiting) && a.seen.elapsed() > timeout {
                a.status = Status::Lost;
                out.push((
                    format!("{} stopped responding", a.name),
                    "heartbeat lost".into(),
                ));
            }
        }
        out
    }

    fn answer(&mut self, id: &str, ok: bool) {
        if let Some(a) = self.agents.get_mut(id) {
            a.approval = Some(ok);
            a.status = if ok { Status::Running } else { Status::Failed };
            if !ok {
                a.message = "approval denied".into();
            }
        }
    }

    fn count(&self, s: Status) -> usize {
        self.agents.values().filter(|a| a.status == s).count()
    }
}

fn notify((title, body): Notice) {
    // Errors ignored: a missing notification daemon must never break monitoring.
    let _ = notify_rust::Notification::new()
        .appname("Agent Control Centre")
        .summary(&title)
        .body(&body)
        .show();
}

fn serve(server: tiny_http::Server, store: Arc<Mutex<Store>>, ctx: egui::Context) {
    for mut req in server.incoming_requests() {
        let method = req.method().clone();
        let path = req.url().to_string();
        let (code, body) = match (method, path.as_str()) {
            (tiny_http::Method::Post, "/event") => {
                let mut buf = String::new();
                // 64 KiB cap so a rogue local process can't make us buffer forever.
                let _ = req.as_reader().take(65536).read_to_string(&mut buf);
                match serde_json::from_str::<Event>(&buf)
                    .map_err(|_| ())
                    .and_then(|e| store.lock().unwrap().apply(e))
                {
                    Ok(n) => {
                        if let Some(n) = n {
                            notify(n);
                        }
                        ctx.request_repaint();
                        (200, json!({"ok": true}))
                    }
                    Err(()) => (400, json!({"error": "bad event"})),
                }
            }
            (tiny_http::Method::Get, "/agents") => {
                let s = store.lock().unwrap();
                let list: Vec<_> = s.agents.iter().map(|(id, a)| json!({"agent_id": id, "status": a.status.label(), "task": a.task, "progress": a.progress, "approval": a.approval})).collect();
                (200, json!(list))
            }
            (tiny_http::Method::Get, p) if p.starts_with("/agents/") => {
                match store.lock().unwrap().agents.get(&p["/agents/".len()..]) {
                    Some(a) => (
                        200,
                        json!({"status": a.status.label(), "approval": a.approval}),
                    ),
                    None => (404, json!({"error": "unknown agent"})),
                }
            }
            _ => (404, json!({"error": "not found"})),
        };
        let h = tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap();
        let _ = req.respond(
            tiny_http::Response::from_string(body.to_string())
                .with_status_code(code)
                .with_header(h),
        );
    }
}

struct App {
    store: Arc<Mutex<Store>>,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_secs(1)); // keep heartbeat ages fresh
        egui::Frame::new().inner_margin(12.0).show(ui, |ui| {
            let mut s = self.store.lock().unwrap();
            s.sweep(HEARTBEAT_TIMEOUT).into_iter().for_each(notify);

            ui.heading("AGENT CONTROL CENTRE");
            let (run, wait, appr) = (
                s.count(Status::Running),
                s.count(Status::Waiting),
                s.count(Status::Approval),
            );
            ui.label(format!(
                "{run} running · {wait} waiting · {appr} need approval · {} failed · {} lost",
                s.count(Status::Failed),
                s.count(Status::Lost)
            ));
            ui.separator();

            let mut answer: Option<(String, bool)> = None;
            for (id, a) in s
                .agents
                .iter()
                .filter(|(_, a)| a.status == Status::Approval)
            {
                egui::Frame::group(ui.style())
                    .fill(Color32::from_rgb(70, 40, 20))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!("Action required: {}", a.name))
                                .strong()
                                .color(a.status.color()),
                        );
                        ui.label(&a.message);
                        ui.horizontal(|ui| {
                            if ui.button("Approve").clicked() {
                                answer = Some((id.clone(), true));
                            }
                            if ui.button("Deny").clicked() {
                                answer = Some((id.clone(), false));
                            }
                        });
                    });
            }
            if let Some((id, ok)) = answer {
                s.answer(&id, ok);
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                if s.agents.is_empty() {
                    ui.label(format!("No agents yet. POST events to http://{ADDR}/event"));
                }
                for a in s.agents.values() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("●").color(a.status.color()));
                        ui.label(RichText::new(&a.name).strong());
                        ui.label(RichText::new(a.status.label()).color(a.status.color()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.weak(format!("{}s ago", a.seen.elapsed().as_secs()));
                        });
                    });
                    ui.label(&a.task);
                    ui.add(
                        egui::ProgressBar::new(a.progress as f32 / 100.0)
                            .text(format!("{}%", a.progress)),
                    );
                    ui.add_space(8.0);
                }
            });

            ui.separator();
            if ui.button("Clear finished").clicked() {
                s.agents
                    .retain(|_, a| !matches!(a.status, Status::Done | Status::Failed));
            }
        });
    }
}

fn main() -> eframe::Result {
    let server = tiny_http::Server::http(ADDR).unwrap_or_else(|e| {
        eprintln!("cannot listen on {ADDR} (already running?): {e}");
        std::process::exit(1);
    });
    let store = Arc::new(Mutex::new(Store::default()));
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Agent Control Centre")
            .with_inner_size([400.0, 560.0])
            .with_always_on_top(),
        ..Default::default()
    };
    let s = store.clone();
    eframe::run_native(
        "Agent Control Centre",
        opts,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || serve(server, s, ctx));
            Ok(Box::new(App { store }))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, id: &str) -> Event {
        Event {
            event: kind.into(),
            agent_id: id.into(),
            name: None,
            task: None,
            message: None,
            progress: None,
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
    }

    #[test]
    fn heartbeat_lost_and_recovered() {
        let mut s = Store::default();
        s.apply(ev("agent.started", "a")).unwrap();
        assert_eq!(s.sweep(Duration::ZERO).len(), 1);
        assert_eq!(s.agents["a"].status, Status::Lost);
        assert!(s.sweep(Duration::ZERO).is_empty()); // notified once only
        s.apply(ev("agent.heartbeat", "a")).unwrap();
        assert_eq!(s.agents["a"].status, Status::Running);
    }
}
