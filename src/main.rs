//! Agent Control Centre: a native, always-on-top HUD that shows every AI agent on
//! this machine, notifies you when one needs attention, and lets you launch,
//! pause, resume and stop them. Agents report over a localhost HTTP API.
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod shell;
mod theme;
mod ui;

use acc_core::procs::{self, Procs, Wake};
use acc_core::store::{Notice, Store};
use acc_core::{api, auth, log};
use eframe::egui;
use serde_json::json;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn os_notify(n: Notice) {
    let mut note = notify_rust::Notification::new();
    note.appname("Agent Control Centre")
        .summary(&n.title)
        .body(&n.body);
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use acc_core::store::Severity;
        note.urgency(match n.severity {
            Severity::Info => notify_rust::Urgency::Low,
            Severity::Warning => notify_rust::Urgency::Normal,
            Severity::Critical => notify_rust::Urgency::Critical,
        });
    }
    // Errors ignored: a missing notification daemon must never break monitoring.
    let _ = note.show();
}

/// Hand queued notices to the OS. The store has already applied the user's preferences.
fn flush(store: &Mutex<Store>) -> bool {
    let out = std::mem::take(&mut store.lock().unwrap().outbox);
    let any = !out.is_empty();
    out.into_iter().for_each(os_notify);
    any
}

/// Stop managed processes and write state. Safe to call more than once.
pub fn shutdown(store: &Mutex<Store>, procs: &Mutex<Procs>) {
    procs.lock().unwrap().stop_all();
    let mut s = store.lock().unwrap();
    s.touch();
    s.save_agents();
    log::info("shutdown", json!({}));
}

/// Set by SIGTERM / SIGINT (logout, `kill`, Ctrl+C). The ticker sees it and shuts down cleanly.
static TERMINATE: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn catch_signals() {
    extern "C" fn on_signal(_: libc::c_int) {
        TERMINATE.store(true, Ordering::SeqCst); // the only thing that is safe to do here
    }
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}
#[cfg(not(unix))]
fn catch_signals() {}

pub fn level(pinned: bool) -> egui::WindowLevel {
    if pinned {
        egui::WindowLevel::AlwaysOnTop
    } else {
        egui::WindowLevel::Normal
    }
}

fn main() -> eframe::Result {
    let started = Instant::now();
    let store = Arc::new(Mutex::new(Store::persistent()));
    let cfg = store.lock().unwrap().cfg.clone();
    let server = tiny_http::Server::http(cfg.addr.as_str()).unwrap_or_else(|e| {
        eprintln!("cannot listen on {} (already running?): {e}", cfg.addr);
        std::process::exit(1);
    });
    let token = auth::load_or_create();
    catch_signals();
    log::info(
        "started",
        json!({"version": env!("CARGO_PKG_VERSION"), "addr": cfg.addr}),
    );
    let opts = eframe::NativeOptions {
        persist_window: true, // remembers size and position between runs
        // Window state lives with the rest of our data, so a second instance (ACC_DATA_DIR) has its own.
        persistence_path: acc_core::config::data_dir().map(|d| d.join("window.ron")),
        viewport: egui::ViewportBuilder::default()
            .with_title("Agent Control Centre")
            .with_inner_size(if cfg.compact { ui::COMPACT } else { ui::FULL })
            .with_min_inner_size(if cfg.compact {
                ui::COMPACT_MIN
            } else {
                ui::FULL_MIN
            })
            .with_icon(egui::IconData {
                rgba: shell::dot_rgba(shell::GREEN),
                width: 64,
                height: 64,
            })
            .with_transparent(cfg.opacity < 1.0)
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
            let (pinned, visible, quitting) = (
                Arc::new(AtomicBool::new(true)),
                Arc::new(AtomicBool::new(true)),
                Arc::new(AtomicBool::new(false)),
            );
            // Background threads repaint only while the window is on screen: hidden in the tray costs nothing.
            let wake: Wake = Arc::new({
                let (ctx, visible) = (ctx.clone(), visible.clone());
                move || {
                    if visible.load(Ordering::SeqCst) {
                        // At most ~10 repaints a second however fast events arrive.
                        ctx.request_repaint_after(Duration::from_millis(100));
                    }
                }
            });
            std::thread::spawn({
                let (s, wake) = (s.clone(), wake.clone());
                move || {
                    api::serve(server, s.clone(), api::Api::new(token), move || {
                        flush(&s);
                        wake();
                    })
                }
            });
            procs::watch(s.clone(), wake.clone());
            let procs = Arc::new(Mutex::new(Procs::default()));

            // Ticker: heartbeat/approval expiry, managed-process exits, the launch queue and saving
            // keep working while the window is hidden. Lock order everywhere: procs, then store.
            std::thread::spawn({
                let (s, procs, wake) = (s.clone(), procs.clone(), wake.clone());
                move || loop {
                    if TERMINATE.load(Ordering::SeqCst) {
                        shutdown(&s, &procs);
                        std::process::exit(0);
                    }
                    let exited = {
                        let mut p = procs.lock().unwrap();
                        let exited = {
                            let mut st = s.lock().unwrap();
                            st.sweep(1.0);
                            let e = p.reap(&mut st);
                            st.save_agents();
                            e
                        };
                        p.drain_queue(&s, &wake);
                        exited
                    };
                    if flush(&s) || exited {
                        wake();
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            });

            // Tray + hotkey commands.
            let (tx, rx) = std::sync::mpsc::channel();
            let shell = shell::start(tx, s.clone(), &cfg.hotkey);
            if cfg.start_hidden && shell.tray.is_ok() {
                visible.store(false, Ordering::SeqCst);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            std::thread::spawn({
                let (ctx, visible, pinned, quitting, s, procs) = (
                    ctx.clone(),
                    visible.clone(),
                    pinned.clone(),
                    quitting.clone(),
                    s.clone(),
                    procs.clone(),
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
                                    ctx.request_repaint();
                                }
                            }
                            shell::Cmd::Pin => {
                                let p = !pinned.fetch_xor(true, Ordering::SeqCst);
                                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level(p)));
                            }
                            shell::Cmd::Quit => {
                                quitting.store(true, Ordering::SeqCst);
                                shutdown(&s, &procs);
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
            log::info(
                "window ready",
                json!({"startup_ms": started.elapsed().as_millis() as u64}),
            );
            Ok(Box::new(ui::App::new(
                store, procs, wake, pinned, visible, quitting, shell,
            )))
        }),
    )
}
