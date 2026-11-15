//! Agent Control Centre: a native, always-on-top HUD that shows every AI agent on
//! this machine, notifies you when one needs attention, and lets you launch,
//! pause, resume and stop them. Agents report over a localhost HTTP API.
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod shell;
mod theme;

use agent_control_centre::procs::{self, CAN_PAUSE, Procs, Sig};
use agent_control_centre::store::{
    ADDR, Agent, Approval, Detected, Entry, Event, Notice, Source, Status, Store, now_secs,
};
use eframe::egui::{self, Color32, Margin, RichText, Stroke};
use serde_json::json;
use std::{
    io::Read,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
    time::Duration,
};
use theme::{ACCENT, BORDER, CARD, DANGER, DIM, FG, MUTED, WELL};

fn status_color(s: Status) -> Color32 {
    match s {
        Status::Running => ACCENT,
        Status::Waiting => Color32::from_rgb(0xF5, 0x9E, 0x0B),
        Status::Approval => Color32::from_rgb(0xFB, 0x92, 0x3C),
        Status::Paused => Color32::from_rgb(0x38, 0xBD, 0xF8),
        Status::Done => Color32::from_rgb(0x81, 0x8C, 0xF8),
        Status::Failed => DANGER,
        Status::Lost => Color32::from_rgb(0xA7, 0x8B, 0xFA),
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
        // Expire stale approvals/heartbeats on every request too, so answers stay correct while the window is hidden.
        let swept = store.lock().unwrap().sweep(1.0);
        swept.into_iter().for_each(notify);
        let (code, body) = match (method, path.as_str()) {
            (tiny_http::Method::Post, "/event") => {
                let mut buf = String::new();
                // 64 KiB cap so a rogue local process can't make us buffer forever.
                let _ = req.as_reader().take(65536).read_to_string(&mut buf);
                match serde_json::from_str::<Event>(&buf)
                    .map_err(|e| e.to_string())
                    .and_then(|e| store.lock().unwrap().apply(e).map_err(String::from))
                {
                    Ok(n) => {
                        if let Some(n) = n {
                            notify(n);
                        }
                        ctx.request_repaint();
                        (200, json!({"ok": true}))
                    }
                    Err(e) => (400, json!({"error": e})),
                }
            }
            (tiny_http::Method::Get, "/agents") => {
                let s = store.lock().unwrap();
                let list: Vec<_> = s
                    .agents
                    .iter()
                    .map(|(id, a)| json!({"agent_id": id, "status": a.status.api(), "task": a.task, "progress": a.progress, "approval": a.approval}))
                    .collect();
                (200, json!(list))
            }
            (tiny_http::Method::Get, "/approvals") => {
                let s = store.lock().unwrap();
                let list: Vec<_> = s
                    .pending_approvals()
                    .map(|a| json!({"approval_id": a.id, "agent_id": a.agent_id, "detail": a.detail, "expires_in": a.remaining().as_secs()}))
                    .collect();
                (200, json!(list))
            }
            (tiny_http::Method::Get, p) if p.starts_with("/approvals/") => {
                match store
                    .lock()
                    .unwrap()
                    .approvals
                    .iter()
                    .find(|a| a.id == p["/approvals/".len()..])
                {
                    Some(a) => (200, json!({"status": a.status()})),
                    None => (404, json!({"error": "unknown approval"})),
                }
            }
            (tiny_http::Method::Get, p) if p.starts_with("/agents/") => {
                match store.lock().unwrap().agents.get(&p["/agents/".len()..]) {
                    Some(a) => (
                        200,
                        json!({"status": a.status.api(), "approval": a.approval}),
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

// ---------- small UI building blocks ----------

fn pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.16))
        .corner_radius(99)
        .inner_margin(Margin::symmetric(9, 2))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .size(11.5)
                    .font(theme::semibold(11.5))
                    .color(color),
            );
        });
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Primary,
    Ghost,
    Danger,
}

fn button(ui: &mut egui::Ui, label: &str, kind: Kind) -> egui::Response {
    let (fill, text) = match kind {
        Kind::Primary => (ACCENT, Color32::from_rgb(0x0F, 0x17, 0x2A)),
        Kind::Ghost => (MUTED, FG),
        Kind::Danger => (
            DANGER.gamma_multiply(0.2),
            Color32::from_rgb(0xFC, 0xA5, 0xA5),
        ),
    };
    ui.add(
        egui::Button::new(RichText::new(label).font(theme::semibold(12.5)).color(text))
            .fill(fill)
            .corner_radius(8),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn card(ui: &mut egui::Ui, accent: Color32, add: impl FnOnce(&mut egui::Ui)) {
    let r = egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(12)
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    let rc = r.response.rect;
    ui.painter().rect_filled(
        egui::Rect::from_min_max(
            rc.left_top() + egui::vec2(0.0, 14.0),
            egui::pos2(rc.left() + 3.0, rc.bottom() - 14.0),
        ),
        2,
        accent,
    );
    ui.add_space(6.0);
}

fn bar(ui: &mut egui::Ui, pct: u8, color: Color32) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 3, WELL);
    let mut fill = rect;
    fill.set_width(rect.width() * pct as f32 / 100.0);
    ui.painter().rect_filled(fill, 3, color);
}

fn ago(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        _ => format!("{}h ago", secs / 3600),
    }
}

fn dim_mono(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(RichText::new(text).monospace().size(11.0).color(DIM));
}

enum Act {
    Answer(String, bool),
    Signal {
        id: Option<String>,
        pid: u32,
        group: bool,
        sig: Sig,
    },
    Clear,
}

fn controls(
    ui: &mut egui::Ui,
    acts: &mut Vec<Act>,
    id: Option<&str>,
    pid: u32,
    group: bool,
    paused: bool,
    finished: bool,
) {
    if finished {
        return;
    }
    ui.horizontal(|ui| {
        if CAN_PAUSE {
            let (label, sig) = if paused {
                ("Resume", Sig::Resume)
            } else {
                ("Pause", Sig::Pause)
            };
            if button(ui, label, Kind::Ghost).clicked() {
                acts.push(Act::Signal {
                    id: id.map(Into::into),
                    pid,
                    group,
                    sig,
                });
            }
        }
        if button(ui, "Stop", Kind::Danger).clicked() {
            acts.push(Act::Signal {
                id: id.map(Into::into),
                pid,
                group,
                sig: Sig::Stop,
            });
        }
    });
}

fn agent_card(ui: &mut egui::Ui, acts: &mut Vec<Act>, s: &Store, id: &str, a: &Agent) {
    let col = status_color(a.status);
    card(ui, col, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&a.name).font(theme::semibold(14.5)));
            ui.label(
                RichText::new(if a.source == Source::Managed {
                    "managed"
                } else {
                    "api"
                })
                .size(10.5)
                .color(DIM),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                pill(ui, a.status.label(), col)
            });
        });
        if !a.task.is_empty() {
            ui.label(RichText::new(&a.task).color(DIM));
        }
        if matches!(a.status, Status::Approval | Status::Failed | Status::Lost)
            && !a.message.is_empty()
        {
            ui.label(RichText::new(&a.message).color(col));
        } else if !a.message.is_empty() {
            ui.label(RichText::new(&a.message).color(DIM).size(12.0));
        }
        if a.progress > 0 || a.status == Status::Done {
            bar(ui, a.progress, col);
        }
        if !a.log.is_empty() {
            egui::Frame::new()
                .fill(WELL)
                .corner_radius(8)
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for l in a
                        .log
                        .iter()
                        .rev()
                        .take(3)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                    {
                        ui.add(
                            egui::Label::new(RichText::new(l).monospace().size(11.0).color(DIM))
                                .truncate(),
                        );
                    }
                });
        }
        let mut meta = format!("seen {}", ago(a.seen.elapsed().as_secs()));
        if let Some(pid) = a.pid {
            meta += &format!(" · pid {pid}");
        }
        dim_mono(ui, meta);
        let hist: Vec<&Entry> = s
            .history
            .iter()
            .rev()
            .filter(|e| e.agent_id == id)
            .collect();
        if !hist.is_empty() {
            let now = now_secs();
            egui::CollapsingHeader::new(
                RichText::new(format!("History ({})", hist.len()))
                    .size(12.0)
                    .color(DIM),
            )
            .id_salt(("hist", id))
            .show(ui, |ui| {
                for e in hist.iter().take(40) {
                    entry_row(ui, e, now, false);
                }
            });
        }
        if let Some(pid) = a.pid {
            controls(
                ui,
                acts,
                Some(id),
                pid,
                a.source == Source::Managed,
                a.status == Status::Paused,
                matches!(a.status, Status::Done | Status::Failed),
            );
        }
    });
}

fn entry_color(event: &str) -> Color32 {
    if event.contains("fail") || event.contains("denied") {
        status_color(Status::Failed)
    } else if event.contains("lost") || event.contains("expired") {
        status_color(Status::Lost)
    } else if event.contains("approval.required") {
        status_color(Status::Approval)
    } else if event.contains("completed") || event.contains("exited") {
        status_color(Status::Done)
    } else {
        ACCENT
    }
}

/// One line of activity history; `with_agent` adds the agent name (global feed).
fn entry_row(ui: &mut egui::Ui, e: &Entry, now: u64, with_agent: bool) {
    let col = entry_color(&e.event);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("●").size(9.0).color(col));
        if with_agent {
            ui.label(RichText::new(&e.agent).font(theme::semibold(13.0)));
        }
        ui.label(
            RichText::new(e.event.replace("agent.", ""))
                .monospace()
                .size(11.0)
                .color(col),
        );
        ui.label(
            RichText::new(ago(now.saturating_sub(e.ts)))
                .monospace()
                .size(11.0)
                .color(DIM),
        );
    });
    if !e.message.is_empty() {
        ui.add(egui::Label::new(RichText::new(&e.message).size(12.0).color(DIM)).truncate());
    }
    ui.add_space(2.0);
}

fn remaining(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn approval_card(ui: &mut egui::Ui, acts: &mut Vec<Act>, ap: &Approval) {
    let col = status_color(Status::Approval);
    card(ui, col, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&ap.agent).font(theme::semibold(14.5)));
            ui.label(RichText::new("is asking").size(11.5).color(DIM));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                pill(
                    ui,
                    &format!("expires {}", remaining(ap.remaining().as_secs())),
                    col,
                )
            });
        });
        if !ap.message.is_empty() {
            ui.label(RichText::new(&ap.message).color(DIM).size(12.0));
        }
        if !ap.detail.is_empty() {
            egui::Frame::new()
                .fill(WELL)
                .corner_radius(8)
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(
                        egui::Label::new(
                            RichText::new(&ap.detail).monospace().size(12.0).color(FG),
                        )
                        .wrap(),
                    );
                });
        }
        ui.horizontal(|ui| {
            if button(ui, "Approve", Kind::Primary).clicked() {
                acts.push(Act::Answer(ap.id.clone(), true));
            }
            if button(ui, "Deny", Kind::Danger).clicked() {
                acts.push(Act::Answer(ap.id.clone(), false));
            }
        });
    });
}

fn detected_card(ui: &mut egui::Ui, acts: &mut Vec<Act>, d: &Detected) {
    let st = if d.paused {
        Status::Paused
    } else {
        Status::Running
    };
    card(ui, status_color(st), |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&d.label).font(theme::semibold(14.5)));
            ui.label(RichText::new("detected").size(10.5).color(DIM));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                pill(ui, st.label(), status_color(st))
            });
        });
        ui.add(
            egui::Label::new(RichText::new(&d.cmd).monospace().size(11.0).color(DIM)).truncate(),
        );
        dim_mono(
            ui,
            format!("pid {} · cpu {:.0}% · {} MB", d.pid, d.cpu, d.mem_mb),
        );
        controls(ui, acts, None, d.pid, false, d.paused, false);
    });
}

fn rank(s: Status) -> u8 {
    match s {
        Status::Approval => 0,
        Status::Failed | Status::Lost => 1,
        Status::Waiting => 2,
        Status::Running | Status::Paused => 3,
        Status::Done => 4,
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Agents,
    Activity,
    Launch,
}

struct App {
    store: Arc<Mutex<Store>>,
    procs: Arc<Mutex<Procs>>,
    pinned: Arc<AtomicBool>,
    visible: Arc<AtomicBool>,
    quitting: Arc<AtomicBool>,
    shell: shell::Shell,
    tab: Tab,
    filter: String, // activity feed: agent id, empty = all
    name: String,
    cmd: String,
    dir: String,
    err: Option<String>,
}

impl App {
    fn run_acts(&mut self, acts: Vec<Act>) {
        let mut s = self.store.lock().unwrap();
        for act in acts {
            match act {
                Act::Clear => s
                    .agents
                    .retain(|_, a| !matches!(a.status, Status::Done | Status::Failed)),
                Act::Answer(id, ok) => s.answer(&id, ok),
                Act::Signal {
                    id,
                    pid,
                    group,
                    sig,
                } => {
                    if !procs::signal(pid, group, sig) {
                        continue;
                    }
                    let Some(a) = id.as_deref().and_then(|i| s.agents.get_mut(i)) else {
                        continue;
                    };
                    match sig {
                        Sig::Pause => a.status = Status::Paused,
                        Sig::Resume => a.status = Status::Running,
                        Sig::Stop => {
                            a.status = Status::Failed;
                            a.message = "stopped by you".into();
                        }
                    }
                }
            }
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, s: &Store) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("Agent Control Centre").font(theme::semibold(19.0)));
                ui.horizontal(|ui| {
                    ui.label(RichText::new("●").size(9.0).color(ACCENT));
                    ui.label(
                        RichText::new(format!("Live · listening on {ADDR}"))
                            .size(11.5)
                            .color(DIM),
                    );
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let pinned = self.pinned.load(Ordering::SeqCst);
                let (label, kind) = if pinned {
                    ("Pinned on top", Kind::Primary)
                } else {
                    ("Pin on top", Kind::Ghost)
                };
                if button(ui, label, kind).clicked() {
                    self.pinned.store(!pinned, Ordering::SeqCst);
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::WindowLevel(level(!pinned)));
                }
            });
        });
        ui.add_space(8.0);
        ui.columns(4, |c| {
            for (i, (label, n, col)) in [
                (
                    "Running",
                    s.count(Status::Running),
                    status_color(Status::Running),
                ),
                (
                    "Waiting",
                    s.count(Status::Waiting),
                    status_color(Status::Waiting),
                ),
                (
                    "Needs you",
                    s.count(Status::Approval),
                    status_color(Status::Approval),
                ),
                (
                    "Failed",
                    s.count(Status::Failed) + s.count(Status::Lost),
                    status_color(Status::Failed),
                ),
            ]
            .into_iter()
            .enumerate()
            {
                egui::Frame::new()
                    .fill(CARD)
                    .stroke(Stroke::new(1.0, BORDER))
                    .corner_radius(10)
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(&mut c[i], |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new(n.to_string())
                                .monospace()
                                .size(20.0)
                                .color(if n > 0 { col } else { DIM }),
                        );
                        ui.label(RichText::new(label).size(11.0).color(DIM));
                    });
            }
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            for (t, label) in [
                (Tab::Agents, "Agents"),
                (Tab::Activity, "Activity"),
                (Tab::Launch, "Launch"),
            ] {
                let on = self.tab == t;
                let text = RichText::new(label)
                    .font(theme::semibold(13.0))
                    .color(if on { ACCENT } else { DIM });
                let b = egui::Button::new(text)
                    .fill(if on {
                        ACCENT.gamma_multiply(0.14)
                    } else {
                        Color32::TRANSPARENT
                    })
                    .corner_radius(8);
                if ui
                    .add(b)
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    self.tab = t;
                }
            }
        });
        ui.add_space(4.0);
    }

    fn agents_tab(&self, ui: &mut egui::Ui, s: &Store, acts: &mut Vec<Act>) {
        let pending: Vec<&Approval> = s.pending_approvals().collect();
        if !pending.is_empty() {
            ui.label(
                RichText::new("NEEDS YOUR APPROVAL")
                    .size(10.5)
                    .font(theme::semibold(10.5))
                    .color(status_color(Status::Approval)),
            );
            for ap in pending {
                approval_card(ui, acts, ap);
            }
            ui.add_space(4.0);
        }
        if s.agents.is_empty() && s.detected.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No agents yet").font(theme::semibold(16.0)));
                ui.label(RichText::new("Launch one from the Launch tab, hook up Claude Code\n(see README), or POST events to the API.").color(DIM));
            });
            return;
        }
        let mut list: Vec<_> = s.agents.iter().collect();
        list.sort_by_key(|(_, a)| rank(a.status));
        for (id, a) in list {
            agent_card(ui, acts, s, id, a);
        }
        if !s.detected.is_empty() {
            ui.add_space(4.0);
            ui.label(
                RichText::new("DETECTED ON THIS MACHINE")
                    .size(10.5)
                    .font(theme::semibold(10.5))
                    .color(DIM),
            );
            for d in &s.detected {
                detected_card(ui, acts, d);
            }
        }
        if s.agents
            .values()
            .any(|a| matches!(a.status, Status::Done | Status::Failed))
            && button(ui, "Clear finished", Kind::Ghost).clicked()
        {
            acts.push(Act::Clear);
        }
    }

    fn activity_tab(&mut self, ui: &mut egui::Ui, s: &Store) {
        let selected = s
            .agents
            .get(&self.filter)
            .map_or("All agents".to_string(), |a| a.name.clone());
        egui::ComboBox::from_id_salt("agent_filter")
            .selected_text(selected)
            .width(200.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.filter, String::new(), "All agents");
                for (id, a) in &s.agents {
                    ui.selectable_value(&mut self.filter, id.clone(), &a.name);
                }
            });
        ui.add_space(4.0);
        let now = now_secs();
        let mut shown = 0;
        for e in s
            .history
            .iter()
            .rev()
            .filter(|e| self.filter.is_empty() || e.agent_id == self.filter)
            .take(200)
        {
            entry_row(ui, e, now, true);
            shown += 1;
        }
        if shown == 0 {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Nothing has happened yet").color(DIM))
            });
        }
    }

    fn launch_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.label(RichText::new("Name").size(12.0).color(DIM));
        ui.add(
            egui::TextEdit::singleline(&mut self.name)
                .hint_text("e.g. Nightly research")
                .desired_width(f32::INFINITY),
        );
        ui.label(RichText::new("Command").size(12.0).color(DIM));
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.cmd)
                .font(egui::TextStyle::Monospace)
                .hint_text("e.g. python agent.py --task refactor")
                .desired_width(f32::INFINITY),
        );
        ui.label(
            RichText::new("Working directory (optional)")
                .size(12.0)
                .color(DIM),
        );
        ui.add(
            egui::TextEdit::singleline(&mut self.dir)
                .font(egui::TextStyle::Monospace)
                .hint_text("defaults to where the HUD was started")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(4.0);
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if button(ui, "Start agent", Kind::Primary).clicked() || enter {
            let ctx = ui.ctx().clone();
            match self.procs.lock().unwrap().spawn(
                &self.store,
                &ctx,
                &self.name,
                &self.cmd,
                &self.dir,
            ) {
                Ok(()) => {
                    self.err = None;
                    self.name.clear();
                    self.cmd.clear();
                    self.tab = Tab::Agents;
                }
                Err(e) => self.err = Some(e),
            }
        }
        if let Some(e) = &self.err {
            ui.label(RichText::new(e).color(DANGER));
        }
        ui.add_space(10.0);
        ui.label(RichText::new("The command runs in your shell. Its output streams into the agent card, and it can report progress with:").size(12.0).color(DIM));
        dim_mono(
            ui,
            "acc emit agent.progress $ACC_AGENT_ID --progress 40 --task \"Indexing\"",
        );
        ui.add_space(14.0);
        ui.label(
            RichText::new("DESKTOP INTEGRATION")
                .size(10.5)
                .font(theme::semibold(10.5))
                .color(DIM),
        );
        match &self.shell.tray {
            Ok(()) => ui.label(RichText::new("Tray: active. Closing the window hides it to the tray; use the tray menu to quit.").size(12.0).color(DIM)),
            Err(e) => ui.label(RichText::new(format!("Tray: unavailable ({e}). Closing the window quits.")).size(12.0).color(status_color(Status::Waiting))),
        };
        match &self.shell.hotkey {
            Ok(l) => ui.label(
                RichText::new(format!(
                    "Hotkey: {l} shows or hides the window (change with ACC_HOTKEY)."
                ))
                .size(12.0)
                .color(DIM),
            ),
            Err(e) => ui.label(
                RichText::new(format!("Hotkey: {e}"))
                    .size(12.0)
                    .color(status_color(Status::Waiting)),
            ),
        };
    }
}

impl eframe::App for App {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        theme::BG.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint_after(Duration::from_secs(1)); // keep ages and heartbeats fresh
        let store = self.store.clone();
        // Closing the window hides it to the tray, but only if a tray really exists (else the user could never get back).
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested())
            && self.shell.tray.is_ok()
            && !self.quitting.load(Ordering::SeqCst)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.visible.store(false, Ordering::SeqCst);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        let mut acts = vec![];
        egui::Frame::new()
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                {
                    let s = store.lock().unwrap();
                    self.header(ui, &s);
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.tab {
                        Tab::Agents => self.agents_tab(ui, &store.lock().unwrap(), &mut acts),
                        Tab::Activity => self.activity_tab(ui, &store.lock().unwrap()),
                        Tab::Launch => self.launch_tab(ui),
                    });
            });
        self.run_acts(acts);
    }
}

fn level(pinned: bool) -> egui::WindowLevel {
    if pinned {
        egui::WindowLevel::AlwaysOnTop
    } else {
        egui::WindowLevel::Normal
    }
}

fn icon() -> egui::IconData {
    egui::IconData {
        rgba: shell::dot_rgba(shell::GREEN),
        width: 64,
        height: 64,
    }
}

fn main() -> eframe::Result {
    let server = tiny_http::Server::http(ADDR).unwrap_or_else(|e| {
        eprintln!("cannot listen on {ADDR} (already running?): {e}");
        std::process::exit(1);
    });
    let store = Arc::new(Mutex::new(Store::persistent()));
    let opts = eframe::NativeOptions {
        persist_window: true, // remembers size and position between runs
        viewport: egui::ViewportBuilder::default()
            .with_title("Agent Control Centre")
            .with_inner_size([430.0, 680.0])
            .with_min_inner_size([360.0, 420.0])
            .with_icon(icon())
            .with_always_on_top(),
        ..Default::default()
    };
    let s = store.clone();
    eframe::run_native(
        "Agent Control Centre",
        opts,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn({
                let (s, ctx) = (s.clone(), ctx.clone());
                move || serve(server, s, ctx)
            });
            procs::watch(s.clone(), ctx.clone());

            let procs = Arc::new(Mutex::new(Procs::default()));
            let (pinned, visible, quitting) = (
                Arc::new(AtomicBool::new(true)),
                Arc::new(AtomicBool::new(true)),
                Arc::new(AtomicBool::new(false)),
            );

            // Background ticker: heartbeat/approval expiry and managed-process exits keep working while the window is hidden in the tray.
            std::thread::spawn({
                let (s, procs, ctx) = (s.clone(), procs.clone(), ctx.clone());
                move || loop {
                    let notices: Vec<Notice> = {
                        let mut st = s.lock().unwrap();
                        let mut n = st.sweep(1.0);
                        n.extend(procs.lock().unwrap().reap(&mut st));
                        n
                    };
                    notices.into_iter().for_each(notify);
                    ctx.request_repaint();
                    std::thread::sleep(Duration::from_secs(1));
                }
            });

            // Tray + hotkey commands.
            let (tx, rx) = std::sync::mpsc::channel();
            let shell = shell::start(tx, s.clone());
            std::thread::spawn({
                let (ctx, visible, pinned, quitting) = (
                    ctx.clone(),
                    visible.clone(),
                    pinned.clone(),
                    quitting.clone(),
                );
                move || {
                    for cmd in rx {
                        match cmd {
                            shell::Cmd::Toggle => {
                                let show = !visible.fetch_xor(true, Ordering::SeqCst);
                                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(show));
                                if show {
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                                }
                            }
                            shell::Cmd::Pin => {
                                let p = !pinned.fetch_xor(true, Ordering::SeqCst);
                                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level(p)));
                            }
                            shell::Cmd::Quit => {
                                quitting.store(true, Ordering::SeqCst);
                                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                std::thread::spawn(|| {
                                    std::thread::sleep(Duration::from_secs(2));
                                    std::process::exit(0); // a hidden window may not process Close
                                });
                            }
                        }
                    }
                }
            });
            Ok(Box::new(App {
                store,
                procs,
                pinned,
                visible,
                quitting,
                shell,
                tab: Tab::Agents,
                filter: String::new(),
                name: String::new(),
                cmd: String::new(),
                dir: String::new(),
                err: None,
            }))
        }),
    )
}
