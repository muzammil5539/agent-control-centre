//! The window: header, nine tabs, compact mode. Reads the store, queues actions, applies them after drawing.
use crate::theme::{self, ACCENT, BORDER, CARD, DANGER, DIM, FG, MUTED, WELL};
use crate::{level, shell};
use acc_core::config::Config;
use acc_core::procs::{self, CAN_PAUSE, Procs, Sig, Spec, Wake};
use acc_core::store::{
    Agent, Approval, Conn, Detected, Entry, Risk, Severity, Source, Status, Store, entry_severity,
    now_secs,
};
use eframe::egui::{self, Color32, Margin, RichText, Stroke};
use std::{
    collections::HashSet,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const FULL: [f32; 2] = [430.0, 680.0];
pub const FULL_MIN: [f32; 2] = [360.0, 420.0];
pub const COMPACT: [f32; 2] = [430.0, 150.0];
pub const COMPACT_MIN: [f32; 2] = [300.0, 120.0];

const AMBER: Color32 = Color32::from_rgb(0xF5, 0x9E, 0x0B);
const ORANGE: Color32 = Color32::from_rgb(0xFB, 0x92, 0x3C);
const VIOLET: Color32 = Color32::from_rgb(0xA7, 0x8B, 0xFA);

fn status_color(s: Status) -> Color32 {
    match s {
        Status::Running => ACCENT,
        Status::Waiting => AMBER,
        Status::Approval => ORANGE,
        Status::Paused => Color32::from_rgb(0x38, 0xBD, 0xF8),
        Status::Done => Color32::from_rgb(0x81, 0x8C, 0xF8),
        Status::Failed => DANGER,
    }
}

fn severity_color(s: Severity) -> Color32 {
    match s {
        Severity::Info => ACCENT,
        Severity::Warning => AMBER,
        Severity::Critical => DANGER,
    }
}

fn risk_color(r: Risk) -> Color32 {
    match r {
        Risk::Low => ACCENT,
        Risk::Medium => AMBER,
        Risk::High => DANGER,
    }
}

// ---------- small UI building blocks ----------

fn pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.16))
        .corner_radius(99)
        .inner_margin(Margin::symmetric(9, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).font(theme::semibold(11.5)).color(color));
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

fn well(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(WELL)
        .corner_radius(8)
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn bar(ui: &mut egui::Ui, pct: f32, color: Color32) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 3, WELL);
    let mut fill = rect;
    fill.set_width(rect.width() * pct.clamp(0.0, 100.0) / 100.0);
    ui.painter().rect_filled(fill, 3, color);
}

/// A small status dot, painted (not a font glyph, so it looks the same with any font set).
fn dot(ui: &mut egui::Ui, color: Color32) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(r.center(), 3.5, color);
}

fn ago(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

fn remaining(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn dim(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(RichText::new(text).size(12.0).color(DIM));
}

fn dim_mono(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.add(egui::Label::new(RichText::new(text).monospace().size(11.0).color(DIM)).truncate());
}

fn section(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.add_space(4.0);
    ui.label(RichText::new(text).font(theme::semibold(10.5)).color(color));
}

fn empty(ui: &mut egui::Ui, title: &str, hint: &str) {
    ui.add_space(24.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(title).font(theme::semibold(16.0)));
        if !hint.is_empty() {
            ui.label(RichText::new(hint).color(DIM));
        }
    });
}

/// Card title: the pills keep their place on the right and a long name is cut short instead of running under them.
fn title_row(ui: &mut egui::Ui, name: &str, tag: &str, pills: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            pills(ui);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(
                    egui::Label::new(RichText::new(name).font(theme::semibold(14.5))).truncate(),
                );
                ui.label(RichText::new(tag).size(10.5).color(DIM));
            });
        });
    });
}

enum Act {
    Answer(String, bool),
    Choose(String, String),
    AllowAlways(String, String), // approval id, exact action
    Control(String, Sig),
    SignalPid(u32, Sig),
    PauseAll(bool),
    Restart(String),
    Remove(String),
    Clear,
    Mute(String, bool),
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Agents,
    Approvals,
    Activity,
    Alerts,
    Resources,
    Workspaces,
    Tasks,
    Launch,
    Settings,
}

const TABS: [(Tab, &str); 9] = [
    (Tab::Agents, "Agents"),
    (Tab::Approvals, "Approvals"),
    (Tab::Activity, "Activity"),
    (Tab::Alerts, "Alerts"),
    (Tab::Resources, "Resources"),
    (Tab::Workspaces, "Workspaces"),
    (Tab::Tasks, "Tasks"),
    (Tab::Launch, "Launch"),
    (Tab::Settings, "Settings"),
];

#[derive(Default)]
struct Filter {
    agent: String, // agent id, empty = all
    kind: usize,
    severity: usize,
    range: usize,
    workspace: String,
    text: String,
}

const KINDS: [&str; 5] = [
    "All events",
    "Approvals",
    "Errors and alerts",
    "Tools and files",
    "Lifecycle",
];
const SEVERITIES: [&str; 3] = ["Any severity", "Warning and up", "Critical only"];
const RANGES: [(&str, u64); 4] = [
    ("All time", u64::MAX),
    ("Last hour", 3600),
    ("Last 24 hours", 86_400),
    ("Last 7 days", 604_800),
];

impl Filter {
    fn matches(&self, e: &Entry, s: &Store, now: u64) -> bool {
        let ev = e.event.as_str();
        (self.agent.is_empty() || e.agent_id == self.agent)
            && match self.kind {
                1 => ev.starts_with("approval") || ev.starts_with("question"),
                2 => entry_severity(ev) > Severity::Info,
                3 => ev.starts_with("tool.") || ev.starts_with("file."),
                4 => ev.starts_with("agent.") || ev.starts_with("control."),
                _ => true,
            }
            && entry_severity(ev) as usize >= self.severity
            && now.saturating_sub(e.ts) <= RANGES[self.range].1
            && (self.workspace.is_empty()
                || s.agents
                    .get(&e.agent_id)
                    .is_some_and(|a| a.workspace == self.workspace))
            && (self.text.is_empty() || {
                let t = self.text.to_lowercase();
                e.message.to_lowercase().contains(&t)
                    || e.agent.to_lowercase().contains(&t)
                    || ev.contains(&t)
            })
    }
}

pub struct App {
    store: Arc<Mutex<Store>>,
    procs: Arc<Mutex<Procs>>,
    wake: Wake,
    pinned: Arc<AtomicBool>,
    visible: Arc<AtomicBool>,
    quitting: Arc<AtomicBool>,
    shell: shell::Shell,
    tab: Tab,
    filter: Filter,
    spec: Spec,
    err: Option<String>,
    confirm: Option<(String, Instant)>, // Stop pressed once; a second press within a few seconds stops
    restartable: HashSet<String>,
    pattern: String,
    exported: Option<Result<String, String>>,
    autostart: bool,
}

/// One line of activity history; `with_agent` adds the agent name (global feed).
fn entry_row(ui: &mut egui::Ui, e: &Entry, now: u64, with_agent: bool) {
    let col = match entry_severity(&e.event) {
        Severity::Critical if e.event == "approval.required" => ORANGE,
        Severity::Critical => DANGER,
        Severity::Warning => VIOLET,
        Severity::Info if e.event.contains("completed") || e.event.contains("exited") => {
            status_color(Status::Done)
        }
        Severity::Info => ACCENT,
    };
    ui.horizontal_wrapped(|ui| {
        dot(ui, col);
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

impl App {
    pub fn new(
        store: Arc<Mutex<Store>>,
        procs: Arc<Mutex<Procs>>,
        wake: Wake,
        pinned: Arc<AtomicBool>,
        visible: Arc<AtomicBool>,
        quitting: Arc<AtomicBool>,
        shell: shell::Shell,
    ) -> Self {
        App {
            store,
            procs,
            wake,
            pinned,
            visible,
            quitting,
            shell,
            tab: Tab::Agents,
            filter: Filter::default(),
            spec: Spec {
                name: String::new(),
                cmd: String::new(),
                dir: String::new(),
            },
            err: None,
            confirm: None,
            restartable: HashSet::new(),
            pattern: String::new(),
            exported: None,
            autostart: shell::autostart_enabled(),
        }
    }

    /// Apply queued actions. Lock order: procs, then store.
    fn run_acts(&mut self, acts: Vec<Act>) {
        if acts.is_empty() {
            return;
        }
        let mut p = self.procs.lock().unwrap();
        let mut restart = vec![];
        {
            let mut s = self.store.lock().unwrap();
            let mut save_cfg = false;
            for act in acts {
                match act {
                    Act::Clear => {
                        s.agents.retain(|_, a| !a.status.finished());
                        s.touch();
                    }
                    Act::Remove(id) => {
                        s.agents.remove(&id);
                        s.touch();
                    }
                    Act::Answer(id, ok) => s.answer(&id, ok),
                    Act::Choose(id, option) => s.choose(&id, &option),
                    Act::AllowAlways(id, action) => {
                        if !s.cfg.allow_patterns.contains(&action) {
                            s.cfg.allow_patterns.push(action);
                            save_cfg = true;
                        }
                        s.answer(&id, true);
                    }
                    Act::Control(id, sig) => {
                        if let Err(e) = procs::control(&mut s, &id, sig) {
                            self.err = Some(format!("{id}: {e}"));
                        }
                    }
                    Act::SignalPid(pid, sig) => {
                        // Only pids the scanner still lists: guards against a pid that was reused meanwhile.
                        if s.detected.iter().any(|d| d.pid == pid)
                            && !procs::signal(pid, false, sig)
                        {
                            self.err =
                                Some(format!("pid {pid}: the signal could not be delivered"));
                        }
                    }
                    Act::PauseAll(pause) => {
                        let sig = if pause { Sig::Pause } else { Sig::Resume };
                        let ids: Vec<String> = s
                            .agents
                            .iter()
                            .filter(|(_, a)| {
                                a.pid.is_some()
                                    && !a.status.finished()
                                    && (a.status == Status::Paused) != pause
                            })
                            .map(|(id, _)| id.clone())
                            .collect();
                        for id in ids {
                            let _ = procs::control(&mut s, &id, sig);
                        }
                        for d in s.detected.iter().filter(|d| d.paused != pause) {
                            procs::signal(d.pid, false, sig);
                        }
                    }
                    Act::Restart(id) => restart.extend(p.spec(&id).cloned()),
                    Act::Mute(id, on) => {
                        s.cfg.muted_agents.retain(|m| *m != id);
                        if on {
                            s.cfg.muted_agents.push(id);
                        }
                        save_cfg = true;
                    }
                }
            }
            if save_cfg && let Err(e) = s.cfg.save() {
                s.problems.push(e);
            }
        }
        for spec in restart {
            if let Err(e) = p.spawn(&self.store, &self.wake, &spec) {
                self.err = Some(e);
            }
        }
    }

    /// Stop needs two presses: the first arms it for a few seconds.
    fn stop_button(&mut self, ui: &mut egui::Ui, key: &str) -> bool {
        let armed = self
            .confirm
            .as_ref()
            .is_some_and(|(k, t)| k == key && t.elapsed() < Duration::from_secs(4));
        if button(
            ui,
            if armed { "Confirm stop" } else { "Stop" },
            Kind::Danger,
        )
        .clicked()
        {
            if armed {
                self.confirm = None;
                return true;
            }
            self.confirm = Some((key.into(), Instant::now()));
        }
        false
    }

    fn controls(
        &mut self,
        ui: &mut egui::Ui,
        acts: &mut Vec<Act>,
        id: Option<&str>,
        pid: u32,
        paused: bool,
    ) {
        let act = |sig| match id {
            Some(id) => Act::Control(id.into(), sig),
            None => Act::SignalPid(pid, sig),
        };
        if CAN_PAUSE {
            let (label, sig) = if paused {
                ("Resume", Sig::Resume)
            } else {
                ("Pause", Sig::Pause)
            };
            if button(ui, label, Kind::Ghost).clicked() {
                acts.push(act(sig));
            }
        }
        if self.stop_button(ui, &format!("{}{pid}", id.unwrap_or("pid"))) {
            acts.push(act(Sig::Stop));
        }
    }

    fn agent_card(
        &mut self,
        ui: &mut egui::Ui,
        acts: &mut Vec<Act>,
        s: &Store,
        id: &str,
        a: &Agent,
    ) {
        let col = if a.conn == Conn::Lost {
            VIOLET
        } else {
            status_color(a.status)
        };
        card(ui, col, |ui| {
            title_row(
                ui,
                &a.name,
                if a.source == Source::Managed {
                    "managed"
                } else {
                    "api"
                },
                |ui| {
                    // Two separate facts: what it was doing, and whether we still hear from it.
                    match a.conn {
                        Conn::Lost => pill(ui, "lost contact", VIOLET),
                        Conn::Stale => pill(ui, "quiet", AMBER),
                        Conn::Connected => {}
                    }
                    match a.requested {
                        Some(Status::Paused) => pill(ui, "pausing…", status_color(Status::Paused)),
                        Some(_) => pill(ui, "resuming…", ACCENT),
                        None => pill(ui, a.status.label(), status_color(a.status)),
                    }
                },
            );
            if !a.task.is_empty() {
                ui.label(RichText::new(&a.task).color(DIM));
            }
            if !a.message.is_empty() {
                if a.troubled() || a.status == Status::Approval {
                    ui.label(RichText::new(&a.message).color(col));
                } else {
                    ui.label(RichText::new(&a.message).color(DIM).size(12.0));
                }
            }
            if a.progress > 0 || a.status == Status::Done {
                bar(ui, a.progress as f32, col);
            }
            if !a.log.is_empty() {
                well(ui, |ui| {
                    for l in a.log.iter().skip(a.log.len().saturating_sub(3)) {
                        dim_mono(ui, l);
                    }
                });
                egui::CollapsingHeader::new(
                    RichText::new(format!("Output ({} lines)", a.log.len()))
                        .size(12.0)
                        .color(DIM),
                )
                .id_salt(("out", id))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt(("outs", id))
                        .max_height(220.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for l in &a.log {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(l).monospace().size(11.0).color(DIM),
                                    )
                                    .wrap(),
                                );
                            }
                        });
                });
            }
            let mut meta = format!("seen {}", ago(a.seen.elapsed().as_secs()));
            if let Some(pid) = a.pid {
                meta += &format!(" · pid {pid}");
            }
            if !a.capabilities.is_empty() {
                meta += &format!(" · {}", a.capabilities.join(", "));
            }
            dim_mono(ui, meta);
            if !a.workspace.is_empty() {
                let git = s
                    .workspaces
                    .get(&a.workspace)
                    .filter(|w| w.git)
                    .map_or(String::new(), |w| {
                        format!(" · {} · {} changed", w.branch, w.changed.len())
                    });
                dim_mono(ui, format!("{}{git}", a.workspace));
            }
            if a.usage != Default::default() {
                dim_mono(
                    ui,
                    format!(
                        "{} in · {} out tokens · ${:.2}",
                        a.usage.tokens_in, a.usage.tokens_out, a.usage.cost_usd
                    ),
                );
            }
            let hist: Vec<&Entry> = s
                .history
                .iter()
                .rev()
                .filter(|e| e.agent_id == id)
                .take(40)
                .collect();
            if !hist.is_empty() {
                let now = now_secs();
                egui::CollapsingHeader::new(RichText::new("History").size(12.0).color(DIM))
                    .id_salt(("hist", id))
                    .show(ui, |ui| {
                        for e in &hist {
                            entry_row(ui, e, now, false);
                        }
                    });
            }
            ui.horizontal_wrapped(|ui| {
                if let Some(pid) = a.pid.filter(|_| !a.status.finished()) {
                    self.controls(ui, acts, Some(id), pid, a.status == Status::Paused);
                }
                if a.status.finished() {
                    if self.restartable.contains(id) && button(ui, "Restart", Kind::Ghost).clicked()
                    {
                        acts.push(Act::Restart(id.into()));
                    }
                    if button(ui, "Remove", Kind::Ghost).clicked() {
                        acts.push(Act::Remove(id.into()));
                    }
                }
                let muted = s.cfg.muted_agents.iter().any(|m| m == id);
                if button(ui, if muted { "Unmute" } else { "Mute" }, Kind::Ghost)
                    .on_hover_text("Muted agents still alert you for approvals and failures")
                    .clicked()
                {
                    acts.push(Act::Mute(id.into(), !muted));
                }
            });
        });
    }

    fn approval_card(&self, ui: &mut egui::Ui, acts: &mut Vec<Act>, ap: &Approval) {
        let col = if ap.options.is_empty() {
            risk_color(ap.risk).lerp_to_gamma(ORANGE, 0.35)
        } else {
            ORANGE
        };
        card(ui, col, |ui| {
            title_row(
                ui,
                &ap.agent,
                if ap.options.is_empty() {
                    "asks to run"
                } else {
                    "asks"
                },
                |ui| {
                    pill(
                        ui,
                        &format!("expires {}", remaining(ap.remaining().as_secs())),
                        ORANGE,
                    );
                    if ap.options.is_empty() {
                        pill(ui, ap.risk.label(), risk_color(ap.risk));
                    }
                },
            );
            if !ap.message.is_empty() {
                dim(ui, &ap.message);
            }
            if !ap.detail.is_empty() {
                well(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&ap.detail).monospace().size(12.0).color(FG),
                        )
                        .wrap(),
                    );
                });
            }
            ui.horizontal_wrapped(|ui| {
                if !ap.options.is_empty() {
                    for o in &ap.options {
                        if button(ui, o, Kind::Ghost).clicked() {
                            acts.push(Act::Choose(ap.id.clone(), o.clone()));
                        }
                    }
                    return;
                }
                if button(ui, "Approve", Kind::Primary).clicked() {
                    acts.push(Act::Answer(ap.id.clone(), true));
                }
                if button(ui, "Deny", Kind::Danger).clicked() {
                    acts.push(Act::Answer(ap.id.clone(), false));
                }
                // High-risk actions are asked every time.
                if ap.risk < Risk::High
                    && button(ui, "Always allow this", Kind::Ghost)
                        .on_hover_text("Approve, and stop asking for this exact action. Manage rules in Settings.")
                        .clicked()
                {
                    acts.push(Act::AllowAlways(ap.id.clone(), ap.detail.clone()));
                }
            });
        });
    }

    fn detected_card(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>, d: &Detected) {
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
            dim_mono(ui, &d.cmd);
            dim_mono(
                ui,
                format!("pid {} · cpu {:.0}% · {} MB", d.pid, d.cpu, d.mem_mb),
            );
            ui.horizontal(|ui| self.controls(ui, acts, None, d.pid, d.paused));
        });
    }

    fn counts(&self, ui: &mut egui::Ui, s: &Store) {
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
                ("Needs you", s.pending_approvals().count(), ORANGE),
                ("Failed / lost", s.troubled(), DANGER),
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
    }

    fn set_compact(&self, ctx: &egui::Context, compact: bool) {
        let (size, min) = if compact {
            (COMPACT, COMPACT_MIN)
        } else {
            (FULL, FULL_MIN)
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(min.into()));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size.into()));
        let mut s = self.store.lock().unwrap();
        s.cfg.compact = compact;
        if let Err(e) = s.cfg.save() {
            s.problems.push(e);
        }
    }

    /// Returns Some(new compact state) when the user toggled it.
    fn header(&mut self, ui: &mut egui::Ui, s: &Store) -> Option<bool> {
        let compact = s.cfg.compact;
        let mut toggle = None;
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Agent Control Centre").font(theme::semibold(if compact {
                        14.0
                    } else {
                        19.0
                    })),
                );
                if !compact {
                    ui.horizontal(|ui| {
                        dot(ui, ACCENT);
                        let sys = if s.system.mem_total_mb > 0 {
                            format!(
                                " · CPU {:.0}% · RAM {:.1}/{:.0} GB",
                                s.system.cpu,
                                s.system.mem_used_mb as f32 / 1024.0,
                                s.system.mem_total_mb as f32 / 1024.0
                            )
                        } else {
                            String::new()
                        };
                        ui.label(
                            RichText::new(format!("Live on {}{sys}", s.cfg.addr))
                                .size(11.5)
                                .color(DIM),
                        );
                    });
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let pinned = self.pinned.load(Ordering::SeqCst);
                if button(
                    ui,
                    if pinned { "Pinned" } else { "Pin" },
                    if pinned { Kind::Primary } else { Kind::Ghost },
                )
                .on_hover_text("Keep the window above other windows")
                .clicked()
                {
                    self.pinned.store(!pinned, Ordering::SeqCst);
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::WindowLevel(level(!pinned)));
                }
                if button(ui, if compact { "Expand" } else { "Compact" }, Kind::Ghost).clicked() {
                    toggle = Some(!compact);
                }
            });
        });
        ui.add_space(if compact { 4.0 } else { 8.0 });
        self.counts(ui, s);
        if compact {
            return toggle;
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(2.0, 2.0);
            for (t, label) in TABS {
                let on = self.tab == t;
                let badge = match t {
                    Tab::Approvals => s.pending_approvals().count(),
                    Tab::Tasks => self.procs.try_lock().map_or(0, |p| p.queue.len()),
                    _ => 0,
                };
                let label = if badge > 0 {
                    format!("{label} {badge}")
                } else {
                    label.to_string()
                };
                let b = egui::Button::new(
                    RichText::new(label)
                        .font(theme::semibold(12.5))
                        .color(if on { ACCENT } else { DIM }),
                )
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
        toggle
    }

    fn agents_tab(&mut self, ui: &mut egui::Ui, s: &Store, acts: &mut Vec<Act>) {
        let pending: Vec<&Approval> = s.pending_approvals().collect();
        if !pending.is_empty() {
            section(ui, "NEEDS YOUR DECISION", ORANGE);
            for ap in pending {
                self.approval_card(ui, acts, ap);
            }
        }
        if s.agents.is_empty() && s.detected.is_empty() {
            if s.scanned {
                empty(
                    ui,
                    "No agents yet",
                    "Launch one from the Launch tab, hook up Claude Code\n(see README), or POST events to the API.",
                );
            } else {
                empty(ui, "Looking for running agents…", "");
            }
            return;
        }
        let controllable = s
            .agents
            .values()
            .any(|a| a.pid.is_some() && !a.status.finished())
            || !s.detected.is_empty();
        if CAN_PAUSE && controllable {
            ui.horizontal(|ui| {
                if button(ui, "Pause all", Kind::Ghost).clicked() {
                    acts.push(Act::PauseAll(true));
                }
                if button(ui, "Resume all", Kind::Ghost).clicked() {
                    acts.push(Act::PauseAll(false));
                }
            });
        }
        let mut list: Vec<_> = s.agents.iter().collect();
        list.sort_by_key(|(_, a)| match a.status {
            Status::Approval => 0,
            _ if a.troubled() => 1,
            Status::Waiting => 2,
            Status::Running | Status::Paused => 3,
            _ => 4,
        });
        for (id, a) in list {
            self.agent_card(ui, acts, s, id, a);
        }
        if !s.detected.is_empty() {
            section(ui, "DETECTED ON THIS MACHINE", DIM);
            for d in &s.detected {
                self.detected_card(ui, acts, d);
            }
        }
        if s.agents.values().any(|a| a.status.finished())
            && button(ui, "Clear finished", Kind::Ghost).clicked()
        {
            acts.push(Act::Clear);
        }
    }

    fn approvals_tab(&self, ui: &mut egui::Ui, s: &Store, acts: &mut Vec<Act>) {
        let pending: Vec<&Approval> = s.pending_approvals().collect();
        if s.approvals.is_empty() {
            empty(
                ui,
                "No approval requests yet",
                "When an agent asks before acting, the exact action shows here.\nNothing is ever approved by a timeout.",
            );
            return;
        }
        section(ui, "WAITING", ORANGE);
        if pending.is_empty() {
            dim(ui, "Nothing is waiting for you.");
        }
        for ap in pending {
            self.approval_card(ui, acts, ap);
        }
        section(ui, "DECISIONS", DIM);
        let now = now_secs();
        for ap in s.approvals.iter().rev().filter(|a| !a.pending()).take(100) {
            let (label, col) = match ap.status() {
                "approved" if ap.auto => ("allowed by rule", ACCENT),
                "approved" => ("approved", ACCENT),
                "denied" => ("denied", DANGER),
                "answered" => ("answered", status_color(Status::Done)),
                _ => ("expired", VIOLET),
            };
            ui.horizontal_wrapped(|ui| {
                dot(ui, col);
                ui.label(RichText::new(&ap.agent).font(theme::semibold(13.0)));
                ui.label(RichText::new(label).monospace().size(11.0).color(col));
                if let Some(c) = &ap.choice {
                    ui.label(RichText::new(c).monospace().size(11.0).color(FG));
                }
                ui.label(
                    RichText::new(ago(now.saturating_sub(ap.decided_ts)))
                        .monospace()
                        .size(11.0)
                        .color(DIM),
                );
            });
            ui.add(
                egui::Label::new(RichText::new(&ap.detail).monospace().size(11.0).color(DIM))
                    .truncate(),
            );
            ui.add_space(2.0);
        }
    }

    fn activity_tab(&mut self, ui: &mut egui::Ui, s: &Store) {
        let f = &mut self.filter;
        ui.horizontal_wrapped(|ui| {
            let selected = s
                .agents
                .get(&f.agent)
                .map_or("All agents".to_string(), |a| a.name.clone());
            egui::ComboBox::from_id_salt("f_agent")
                .selected_text(selected)
                .width(150.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut f.agent, String::new(), "All agents");
                    for (id, a) in &s.agents {
                        ui.selectable_value(&mut f.agent, id.clone(), &a.name);
                    }
                });
            egui::ComboBox::from_id_salt("f_kind")
                .selected_text(KINDS[f.kind])
                .width(130.0)
                .show_ui(ui, |ui| {
                    for (i, k) in KINDS.iter().enumerate() {
                        ui.selectable_value(&mut f.kind, i, *k);
                    }
                });
            egui::ComboBox::from_id_salt("f_sev")
                .selected_text(SEVERITIES[f.severity])
                .width(120.0)
                .show_ui(ui, |ui| {
                    for (i, k) in SEVERITIES.iter().enumerate() {
                        ui.selectable_value(&mut f.severity, i, *k);
                    }
                });
            egui::ComboBox::from_id_salt("f_range")
                .selected_text(RANGES[f.range].0)
                .width(110.0)
                .show_ui(ui, |ui| {
                    for (i, k) in RANGES.iter().enumerate() {
                        ui.selectable_value(&mut f.range, i, k.0);
                    }
                });
            if !s.workspaces.is_empty() {
                let label = if f.workspace.is_empty() {
                    "Any workspace"
                } else {
                    f.workspace.rsplit('/').next().unwrap_or("")
                };
                egui::ComboBox::from_id_salt("f_ws")
                    .selected_text(label)
                    .width(130.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut f.workspace, String::new(), "Any workspace");
                        for w in s.workspaces.keys() {
                            ui.selectable_value(&mut f.workspace, w.clone(), w);
                        }
                    });
            }
        });
        ui.add(
            egui::TextEdit::singleline(&mut f.text)
                .hint_text("Search activity")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(4.0);
        let now = now_secs();
        let mut shown = 0;
        for e in s
            .history
            .iter()
            .rev()
            .filter(|e| f.matches(e, s, now))
            .take(300)
        {
            entry_row(ui, e, now, true);
            shown += 1;
        }
        if shown == 0 {
            if s.history.is_empty() {
                empty(ui, "Nothing has happened yet", "");
            } else {
                empty(ui, "Nothing matches these filters", "");
            }
        }
    }

    fn alerts_tab(&mut self, ui: &mut egui::Ui, s: &Store, cfg: &mut Config) {
        let now = now_secs();
        ui.horizontal_wrapped(|ui| {
            if now < cfg.snooze_until {
                pill(
                    ui,
                    &format!("snoozed for {}", remaining(cfg.snooze_until - now)),
                    AMBER,
                );
                if button(ui, "Resume alerts", Kind::Ghost).clicked() {
                    cfg.snooze_until = 0;
                }
            } else {
                for (label, secs) in [("Snooze 30 min", 1800), ("Snooze 2 h", 7200)] {
                    if button(ui, label, Kind::Ghost).clicked() {
                        cfg.snooze_until = now + secs;
                    }
                }
            }
        });
        dim(ui, "Snooze and mute never hide approvals or failures.");
        ui.add_space(4.0);
        if s.notes.is_empty() {
            empty(
                ui,
                "No notifications yet",
                "Everything the HUD has alerted you about is listed here.",
            );
            return;
        }
        for n in s.notes.iter().rev().take(200) {
            let col = severity_color(n.severity);
            ui.horizontal_wrapped(|ui| {
                dot(ui, col);
                ui.label(RichText::new(&n.title).font(theme::semibold(13.0)));
                ui.label(
                    RichText::new(n.severity.label())
                        .monospace()
                        .size(11.0)
                        .color(col),
                );
                if !n.shown {
                    ui.label(RichText::new("silenced").monospace().size(11.0).color(DIM));
                }
                ui.label(
                    RichText::new(ago(now.saturating_sub(n.ts)))
                        .monospace()
                        .size(11.0)
                        .color(DIM),
                );
            });
            if !n.body.is_empty() {
                ui.add(egui::Label::new(RichText::new(&n.body).size(12.0).color(DIM)).truncate());
            }
            ui.add_space(2.0);
        }
    }

    fn resources_tab(&self, ui: &mut egui::Ui, s: &Store) {
        if !s.scanned {
            empty(ui, "Measuring…", "");
            return;
        }
        section(ui, "THIS MACHINE", DIM);
        if s.system.mem_total_mb == 0 {
            dim(ui, "System load is not available on this platform.");
        } else {
            ui.label(format!("CPU {:.0}%", s.system.cpu));
            bar(
                ui,
                s.system.cpu,
                if s.system.cpu > 85.0 { DANGER } else { ACCENT },
            );
            let pct = s.system.mem_used_mb as f32 * 100.0 / s.system.mem_total_mb as f32;
            ui.label(format!(
                "Memory {:.1} of {:.1} GB",
                s.system.mem_used_mb as f32 / 1024.0,
                s.system.mem_total_mb as f32 / 1024.0
            ));
            bar(ui, pct, if pct > 90.0 { DANGER } else { ACCENT });
        }
        section(ui, "AGENT PROCESSES, BUSIEST FIRST", DIM);
        let mut procs: Vec<&Detected> = s.detected.iter().collect();
        procs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.mem_mb.cmp(&a.mem_mb)));
        if procs.is_empty() {
            dim(ui, "No agent processes detected.");
        }
        for d in procs {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&d.label).font(theme::semibold(13.0)));
                ui.label(
                    RichText::new(format!(
                        "pid {} · cpu {:.0}% · {} MB",
                        d.pid, d.cpu, d.mem_mb
                    ))
                    .monospace()
                    .size(11.0)
                    .color(DIM),
                );
            });
        }
        section(ui, "REPORTED USAGE, MOST EXPENSIVE FIRST", DIM);
        let mut used: Vec<&Agent> = s
            .agents
            .values()
            .filter(|a| a.usage != Default::default())
            .collect();
        used.sort_by(|a, b| {
            b.usage
                .cost_usd
                .total_cmp(&a.usage.cost_usd)
                .then(b.usage.tokens_out.cmp(&a.usage.tokens_out))
        });
        if used.is_empty() {
            dim(
                ui,
                "No agent has reported tokens or cost. Agents report them with the resource.updated event.",
            );
        } else {
            let total: f64 = used.iter().map(|a| a.usage.cost_usd).sum();
            let budget = if s.cfg.budget_usd > 0.0 {
                format!(" · budget ${:.2} per agent", s.cfg.budget_usd)
            } else {
                String::new()
            };
            dim(ui, format!("Total ${total:.2}{budget}"));
        }
        for a in used {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&a.name).font(theme::semibold(13.0)));
                ui.label(
                    RichText::new(format!(
                        "${:.2} · {} in · {} out · {} ms",
                        a.usage.cost_usd, a.usage.tokens_in, a.usage.tokens_out, a.usage.latency_ms
                    ))
                    .monospace()
                    .size(11.0)
                    .color(DIM),
                );
            });
        }
    }

    fn workspaces_tab(&self, ui: &mut egui::Ui, s: &Store) {
        let now = now_secs();
        if !s.conflicts.is_empty() {
            section(ui, "SAME FILE CHANGED BY TWO AGENTS", AMBER);
            for c in s.conflicts.iter().rev().take(20) {
                ui.horizontal_wrapped(|ui| {
                    dot(ui, AMBER);
                    ui.label(
                        RichText::new(format!("{} and {}", c.first, c.second))
                            .font(theme::semibold(13.0)),
                    );
                    ui.label(
                        RichText::new(ago(now.saturating_sub(c.ts)))
                            .monospace()
                            .size(11.0)
                            .color(DIM),
                    );
                });
                dim_mono(ui, &c.path);
            }
        }
        let dirs: HashSet<&str> = s
            .agents
            .values()
            .map(|a| a.workspace.as_str())
            .filter(|w| !w.is_empty())
            .collect();
        if dirs.is_empty() {
            empty(
                ui,
                "No workspaces yet",
                "Agents report the directory they work in.\nClaude Code hooks and launched agents do it automatically.",
            );
            return;
        }
        let mut dirs: Vec<&str> = dirs.into_iter().collect();
        dirs.sort();
        for d in dirs {
            let here: Vec<&Agent> = s.agents.values().filter(|a| a.workspace == d).collect();
            let active = here
                .iter()
                .filter(|a| !a.status.finished() && a.conn != Conn::Lost)
                .count();
            card(ui, if active > 1 { AMBER } else { ACCENT }, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(d.rsplit(['/', '\\']).next().unwrap_or(d))
                            .font(theme::semibold(14.5)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if active > 1 {
                            pill(ui, &format!("{active} agents active here"), AMBER);
                        }
                    });
                });
                dim_mono(ui, d);
                dim(
                    ui,
                    here.iter()
                        .map(|a| a.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                );
                match s.workspaces.get(d) {
                    None => dim(ui, "Reading git status…"),
                    Some(w) if !w.git => dim(ui, "Not a git repository."),
                    Some(w) => {
                        dim(
                            ui,
                            format!(
                                "Branch {} · {} uncommitted change(s)",
                                w.branch,
                                w.changed.len()
                            ),
                        );
                        for f in w.changed.iter().take(8) {
                            dim_mono(ui, f);
                        }
                        if w.changed.len() > 8 {
                            dim(ui, format!("and {} more", w.changed.len() - 8));
                        }
                    }
                }
            });
        }
    }

    fn tasks_tab(&mut self, ui: &mut egui::Ui, max_parallel: usize) {
        let mut p = self.procs.lock().unwrap();
        dim(
            ui,
            format!(
                "{} of {max_parallel} slots in use. Queued tasks start as slots free up (change the limit in Settings).",
                p.running()
            ),
        );
        if p.queue.is_empty() {
            empty(
                ui,
                "The queue is empty",
                "Use “Add to queue” on the Launch tab.",
            );
            return;
        }
        let mut remove = None;
        for (i, t) in p.queue.iter().enumerate() {
            card(ui, DIM, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if t.name.is_empty() { &t.cmd } else { &t.name })
                            .font(theme::semibold(14.0)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, "Remove", Kind::Ghost).clicked() {
                            remove = Some(i);
                        }
                        pill(ui, &format!("#{}", i + 1), DIM);
                    });
                });
                dim_mono(ui, &t.cmd);
            });
        }
        if let Some(i) = remove {
            p.queue.remove(i);
        }
    }

    fn launch_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        dim(ui, "Name");
        ui.add(
            egui::TextEdit::singleline(&mut self.spec.name)
                .hint_text("e.g. Nightly research")
                .desired_width(f32::INFINITY),
        );
        dim(ui, "Command");
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.spec.cmd)
                .font(egui::TextStyle::Monospace)
                .hint_text("e.g. python agent.py --task refactor")
                .desired_width(f32::INFINITY),
        );
        dim(ui, "Working directory (optional)");
        ui.add(
            egui::TextEdit::singleline(&mut self.spec.dir)
                .font(egui::TextStyle::Monospace)
                .hint_text("defaults to where the HUD was started")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(4.0);
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let (mut start, mut queue) = (enter, false);
        ui.horizontal(|ui| {
            start |= button(ui, "Start agent", Kind::Primary).clicked();
            queue = button(ui, "Add to queue", Kind::Ghost).clicked();
        });
        if start {
            match self
                .procs
                .lock()
                .unwrap()
                .spawn(&self.store, &self.wake, &self.spec)
            {
                Ok(_) => {
                    self.err = None;
                    self.spec.name.clear();
                    self.spec.cmd.clear();
                    self.tab = Tab::Agents;
                }
                Err(e) => self.err = Some(e),
            }
        } else if queue {
            if self.spec.cmd.trim().is_empty() {
                self.err = Some("Enter a command to run".into());
            } else {
                self.procs
                    .lock()
                    .unwrap()
                    .queue
                    .push_back(self.spec.clone());
                self.err = None;
                self.spec.name.clear();
                self.spec.cmd.clear();
                self.tab = Tab::Tasks;
            }
        }
        ui.add_space(10.0);
        dim(
            ui,
            "The command runs in your shell. Its output streams into the agent card, and it can report progress with:",
        );
        dim_mono(
            ui,
            "acc emit agent.progress $ACC_AGENT_ID --progress 40 --task \"Indexing\"",
        );
        dim(
            ui,
            "To watch a command you start in your own terminal instead:",
        );
        dim_mono(ui, "acc run --name \"Codex\" -- codex");
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui, cfg: &mut Config) {
        let num = |ui: &mut egui::Ui, label: &str, hint: &str, w: egui::DragValue| {
            ui.horizontal(|ui| {
                ui.add(w);
                ui.label(label).on_hover_text(hint);
            });
        };
        section(ui, "WINDOW", DIM);
        ui.checkbox(&mut cfg.start_hidden, "Start hidden in the tray");
        if ui
            .checkbox(&mut self.autostart, "Start when I log in")
            .changed()
            && let Err(e) = shell::set_autostart(self.autostart)
        {
            self.err = Some(format!("Autostart: {e}"));
            self.autostart = shell::autostart_enabled();
        }
        ui.horizontal(|ui| {
            ui.add(egui::Slider::new(&mut cfg.opacity, 0.3..=1.0).show_value(true));
            ui.label("Opacity").on_hover_text(
                "Applies the next time the HUD starts. Needs a compositing desktop.",
            );
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut cfg.hotkey)
                    .desired_width(150.0)
                    .font(egui::TextStyle::Monospace),
            );
            ui.label("Show / hide hotkey")
                .on_hover_text("For example ctrl+alt+KeyA. Applies the next time the HUD starts.");
        });
        match &self.shell.tray {
            Ok(()) => dim(
                ui,
                "Tray: active. Closing the window hides it to the tray; use the tray menu to quit.",
            ),
            Err(e) => {
                ui.label(
                    RichText::new(format!(
                        "Tray: unavailable ({e}). Closing the window quits."
                    ))
                    .size(12.0)
                    .color(AMBER),
                );
            }
        }
        match &self.shell.hotkey {
            Ok(l) => dim(ui, format!("Hotkey: {l} is registered.")),
            Err(e) => {
                ui.label(
                    RichText::new(format!("Hotkey: {e}"))
                        .size(12.0)
                        .color(AMBER),
                );
            }
        }

        section(ui, "NOTIFICATIONS", DIM);
        egui::ComboBox::from_label("Show notifications from")
            .selected_text(cfg.min_severity.label())
            .show_ui(ui, |ui| {
                for s in [Severity::Info, Severity::Warning, Severity::Critical] {
                    ui.selectable_value(&mut cfg.min_severity, s, s.label());
                }
            });
        dim(
            ui,
            "Approvals and failures are always shown. Mute single agents from their card.",
        );

        section(ui, "ALERTS (0 = OFF)", DIM);
        num(
            ui,
            "% CPU by one agent process",
            "",
            egui::DragValue::new(&mut cfg.cpu_alert).range(0.0..=6400.0),
        );
        num(
            ui,
            "MB of memory by one agent process",
            "",
            egui::DragValue::new(&mut cfg.mem_alert_mb).range(0..=1_000_000),
        );
        num(
            ui,
            "minutes an agent may run",
            "",
            egui::DragValue::new(&mut cfg.runaway_mins).range(0..=100_000),
        );
        num(
            ui,
            "USD an agent may report spending",
            "",
            egui::DragValue::new(&mut cfg.budget_usd)
                .range(0.0..=100_000.0)
                .speed(0.1),
        );

        section(ui, "APPROVALS", DIM);
        egui::ComboBox::from_label("Claude Code gate (default for acc hook)")
            .selected_text(cfg.gate.as_str())
            .show_ui(ui, |ui| {
                for g in ["off", "bash", "mutating", "all"] {
                    ui.selectable_value(&mut cfg.gate, g.to_string(), g);
                }
            });
        num(
            ui,
            "seconds to wait for your answer",
            "",
            egui::DragValue::new(&mut cfg.gate_timeout).range(5..=3600),
        );
        dim(
            ui,
            "Actions allowed without asking (* matches anything). High-risk actions are always asked.",
        );
        let mut drop = None;
        for (i, p) in cfg.allow_patterns.iter().enumerate() {
            ui.horizontal(|ui| {
                if button(ui, "Remove", Kind::Ghost).clicked() {
                    drop = Some(i);
                }
                dim_mono(ui, p);
            });
        }
        if let Some(i) = drop {
            cfg.allow_patterns.remove(i);
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.pattern)
                    .hint_text("$ cargo test*")
                    .desired_width(220.0)
                    .font(egui::TextStyle::Monospace),
            );
            if button(ui, "Add rule", Kind::Ghost).clicked() && !self.pattern.trim().is_empty() {
                cfg.allow_patterns
                    .push(std::mem::take(&mut self.pattern).trim().to_string());
            }
        });

        section(ui, "DATA", DIM);
        num(
            ui,
            "days of history to keep",
            "",
            egui::DragValue::new(&mut cfg.retention_days).range(1..=3650),
        );
        num(
            ui,
            "hours before finished agents leave the board (0 = never)",
            "",
            egui::DragValue::new(&mut cfg.stale_hours).range(0..=8760),
        );
        if button(ui, "Export diagnostic report", Kind::Ghost)
            .on_hover_text("Agents, approvals, history and notifications as one JSON file. Credentials are redacted.")
            .clicked()
        {
            self.exported = Some(self.store.lock().unwrap().export_to_file().map(|p| p.display().to_string()));
        }
        match &self.exported {
            Some(Ok(p)) => dim_mono(ui, format!("Saved {p}")),
            Some(Err(e)) => {
                ui.label(RichText::new(format!("Export failed: {e}")).color(DANGER));
            }
            None => {}
        }
        if let Some(d) = acc_core::config::data_dir() {
            dim_mono(ui, format!("Data and config: {}", d.display()));
        }

        section(ui, "LOCAL API", DIM);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut cfg.addr)
                    .desired_width(150.0)
                    .font(egui::TextStyle::Monospace),
            );
            ui.label("Listen address")
                .on_hover_text("Loopback only. Applies the next time the HUD starts.");
        });
        if !cfg
            .addr
            .parse::<std::net::SocketAddr>()
            .is_ok_and(|a| a.ip().is_loopback())
        {
            ui.label(
                RichText::new(
                    "Not a loopback address such as 127.0.0.1:7777. It will not be saved.",
                )
                .size(12.0)
                .color(DANGER),
            );
        }
        ui.checkbox(
            &mut cfg.require_registration,
            "Only accept agents that registered first",
        );
        num(
            ui,
            "requests per second",
            "",
            egui::DragValue::new(&mut cfg.rate_limit).range(1..=100_000),
        );
        num(
            ui,
            "queued launches may run at once",
            "",
            egui::DragValue::new(&mut cfg.max_parallel).range(1..=64),
        );
    }
}

impl eframe::App for App {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        let o = self.store.lock().unwrap().cfg.opacity;
        let [r, g, b, _] = theme::BG.to_normalized_gamma_f32();
        [r * o, g * o, b * o, o] // premultiplied
    }

    /// Window size and position are written soon after they change, so they survive a kill or a logout.
    fn auto_save_interval(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        crate::shutdown(&self.store, &self.procs);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint_after(Duration::from_secs(1)); // keep ages and countdowns fresh while visible
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
        self.restartable = self.procs.lock().unwrap().restartable();

        let mut acts = vec![];
        let mut toggle = None;
        let mut dismiss = false;
        let before = store.lock().unwrap().cfg.clone();
        let mut cfg = before.clone();
        egui::Frame::new()
            .inner_margin(Margin::same(if before.compact { 10 } else { 14 }))
            .show(ui, |ui| {
                {
                    let s = store.lock().unwrap();
                    toggle = self.header(ui, &s);
                    let mut problems: Vec<String> = s.problems.clone();
                    problems.extend(self.err.clone());
                    if !problems.is_empty() && !s.cfg.compact {
                        card(ui, DANGER, |ui| {
                            for p in &problems {
                                ui.label(
                                    RichText::new(p)
                                        .size(12.0)
                                        .color(Color32::from_rgb(0xFC, 0xA5, 0xA5)),
                                );
                            }
                            dismiss = button(ui, "Dismiss", Kind::Ghost).clicked();
                        });
                    }
                }
                if before.compact {
                    let s = store.lock().unwrap();
                    if let Some(ap) = s.pending_approvals().next() {
                        ui.add_space(4.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("{} asks: {}", ap.agent, ap.detail))
                                    .size(12.0)
                                    .color(ORANGE),
                            )
                            .truncate(),
                        );
                    }
                    return;
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.tab {
                        Tab::Agents => self.agents_tab(ui, &store.lock().unwrap(), &mut acts),
                        Tab::Approvals => self.approvals_tab(ui, &store.lock().unwrap(), &mut acts),
                        Tab::Activity => self.activity_tab(ui, &store.lock().unwrap()),
                        Tab::Alerts => self.alerts_tab(ui, &store.lock().unwrap(), &mut cfg),
                        Tab::Resources => self.resources_tab(ui, &store.lock().unwrap()),
                        Tab::Workspaces => self.workspaces_tab(ui, &store.lock().unwrap()),
                        Tab::Tasks => self.tasks_tab(ui, before.max_parallel),
                        Tab::Launch => self.launch_tab(ui),
                        Tab::Settings => self.settings_tab(ui, &mut cfg),
                    });
            });
        if dismiss {
            store.lock().unwrap().problems.clear();
            self.err = None;
        }
        if cfg != before {
            let mut s = store.lock().unwrap();
            let ok_addr = cfg
                .addr
                .parse::<std::net::SocketAddr>()
                .is_ok_and(|a| a.ip().is_loopback());
            s.cfg = cfg.clone();
            // Never write an address that would expose the API beyond this machine.
            let to_save = if ok_addr {
                cfg
            } else {
                Config {
                    addr: before.addr.clone(),
                    ..cfg
                }
            };
            if let Err(e) = to_save.save() {
                s.problems.push(e);
            }
        }
        self.run_acts(acts);
        if let Some(c) = toggle {
            self.set_compact(&ctx, c);
        }
    }
}
