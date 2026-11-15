//! Desktop integration: system tray, global show/hide hotkey, minimise-to-tray.
//! Linux uses the StatusNotifierItem D-Bus protocol (`ksni`, no GTK needed); Windows uses `tray-icon`.
use agent_control_centre::store::Store;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState, hotkey::HotKey};
use std::sync::{Arc, Mutex, mpsc::Sender};

pub const DEFAULT_HOTKEY: &str = "ctrl+alt+KeyA";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cmd {
    Toggle,
    Pin,
    Quit,
}

/// A 64x64 status dot (`rgb`) on a dark tile, as RGBA bytes. Used for the window and tray icons.
pub fn dot_rgba(rgb: [u8; 3]) -> Vec<u8> {
    let n = 64usize;
    let mut out = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 - 31.5, y as f32 - 31.5);
            let d = (dx * dx + dy * dy).sqrt();
            let c = if d < 13.0 {
                Some(rgb)
            } else if d < 20.0 {
                Some([rgb[0] / 4, rgb[1] / 4, rgb[2] / 4])
            } else if x > 3 && y > 3 && x < 60 && y < 60 {
                Some([0x0F, 0x17, 0x2A])
            } else {
                None
            };
            if let Some([r, g, b]) = c {
                out[(y * n + x) * 4..][..4].copy_from_slice(&[r, g, b, 255]);
            }
        }
    }
    out
}

pub const GREEN: [u8; 3] = [0x22, 0xC5, 0x5E];
#[cfg(target_os = "linux")]
pub const AMBER: [u8; 3] = [0xFB, 0x92, 0x3C];

/// Human label for a hotkey spec like "ctrl+alt+KeyA" → "Ctrl+Alt+A".
pub fn hotkey_label(spec: &str) -> String {
    spec.split('+')
        .map(|t| {
            let t = t.trim();
            let t = t.strip_prefix("Key").unwrap_or(t);
            let mut c = t.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Keeps the tray and hotkey registrations alive for the life of the app.
pub struct Shell {
    pub tray: Result<(), String>,
    pub hotkey: Result<String, String>, // Ok(label) / Err(reason)
    _hk: Option<GlobalHotKeyManager>,
    #[cfg(windows)]
    _tray: Option<tray_icon::TrayIcon>,
}

fn register_hotkey(tx: Sender<Cmd>) -> (Result<String, String>, Option<GlobalHotKeyManager>) {
    let spec = std::env::var("ACC_HOTKEY").unwrap_or_else(|_| DEFAULT_HOTKEY.into());
    let res = (|| {
        let hk: HotKey = spec
            .parse()
            .map_err(|e| format!("bad ACC_HOTKEY '{spec}': {e}"))?;
        let mgr = GlobalHotKeyManager::new()
            .map_err(|e| format!("global hotkeys unavailable here ({e}); X11/Windows only"))?;
        mgr.register(hk)
            .map_err(|e| format!("{} is taken or not allowed ({e})", hotkey_label(&spec)))?;
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            if e.id == hk.id() && e.state == HotKeyState::Pressed {
                let _ = tx.send(Cmd::Toggle);
            }
        }));
        Ok(mgr)
    })();
    match res {
        Ok(m) => (Ok(hotkey_label(&spec)), Some(m)),
        Err(e) => (Err(e), None),
    }
}

/// Must be called on the main (event-loop) thread, after the window exists.
pub fn start(tx: Sender<Cmd>, store: Arc<Mutex<Store>>) -> Shell {
    let (hotkey, _hk) = register_hotkey(tx.clone());
    #[cfg(target_os = "linux")]
    {
        let tray = linux::spawn(tx, store);
        Shell { tray, hotkey, _hk }
    }
    #[cfg(windows)]
    {
        let _ = store; // tooltip is static on Windows
        match windows::spawn(tx) {
            Ok(t) => Shell {
                tray: Ok(()),
                hotkey,
                _hk,
                _tray: Some(t),
            },
            Err(e) => Shell {
                tray: Err(e),
                hotkey,
                _hk,
                _tray: None,
            },
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (tx, store);
        Shell {
            tray: Err("no tray support on this OS yet".into()),
            hotkey,
            _hk,
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use ksni::blocking::TrayMethods;
    use ksni::{Icon, MenuItem, Status, ToolTip, menu::StandardItem};

    struct T {
        tx: Sender<Cmd>,
        pending: usize,
    }

    fn argb(rgb: [u8; 3]) -> Vec<Icon> {
        let mut d = dot_rgba(rgb);
        for p in d.chunks_exact_mut(4) {
            p.rotate_right(1); // RGBA -> ARGB
        }
        vec![Icon {
            width: 64,
            height: 64,
            data: d,
        }]
    }

    impl ksni::Tray for T {
        fn id(&self) -> String {
            "agent-control-centre".into()
        }
        fn title(&self) -> String {
            "Agent Control Centre".into()
        }
        fn status(&self) -> Status {
            if self.pending > 0 {
                Status::NeedsAttention
            } else {
                Status::Active
            }
        }
        fn icon_pixmap(&self) -> Vec<Icon> {
            argb(GREEN)
        }
        fn attention_icon_pixmap(&self) -> Vec<Icon> {
            argb(AMBER)
        }
        fn tool_tip(&self) -> ToolTip {
            let text = if self.pending == 0 {
                "No approvals waiting".to_string()
            } else {
                format!("{} approval(s) waiting for you", self.pending)
            };
            ToolTip {
                title: "Agent Control Centre".into(),
                description: text,
                ..Default::default()
            }
        }
        fn activate(&mut self, _x: i32, _y: i32) {
            let _ = self.tx.send(Cmd::Toggle);
        }
        fn menu(&self) -> Vec<MenuItem<Self>> {
            let item = |label: &str, cmd: Cmd| {
                StandardItem {
                    label: label.into(),
                    activate: Box::new(move |t: &mut T| {
                        let _ = t.tx.send(cmd);
                    }),
                    ..Default::default()
                }
                .into()
            };
            vec![
                item("Show / Hide", Cmd::Toggle),
                item("Pin on top (toggle)", Cmd::Pin),
                MenuItem::Separator,
                item("Quit", Cmd::Quit),
            ]
        }
    }

    pub fn spawn(tx: Sender<Cmd>, store: Arc<Mutex<Store>>) -> Result<(), String> {
        let handle = T { tx, pending: 0 }
            .spawn()
            .map_err(|e| format!("no StatusNotifier tray on this desktop ({e})"))?;
        // Reflect pending approvals in the tray icon (amber, "needs attention") and tooltip.
        std::thread::spawn(move || {
            let mut last = 0;
            loop {
                let n = store.lock().unwrap().pending_approvals().count();
                if n != last {
                    last = n;
                    let _ = handle.update(|t: &mut T| t.pending = n);
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        });
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    };

    pub fn spawn(tx: Sender<Cmd>) -> Result<TrayIcon, String> {
        let menu = Menu::new();
        menu.append_items(&[
            &MenuItem::with_id("toggle", "Show / Hide", true, None),
            &MenuItem::with_id("pin", "Pin on top (toggle)", true, None),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id("quit", "Quit", true, None),
        ])
        .map_err(|e| e.to_string())?;
        let t2 = tx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let _ = t2.send(match e.id.0.as_str() {
                "pin" => Cmd::Pin,
                "quit" => Cmd::Quit,
                _ => Cmd::Toggle,
            });
        }));
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                let _ = tx.send(Cmd::Toggle);
            }
        }));
        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("Agent Control Centre")
            .with_icon(Icon::from_rgba(dot_rgba(GREEN), 64, 64).map_err(|e| e.to_string())?)
            .build()
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_hotkey_parses_and_labels() {
        assert!(DEFAULT_HOTKEY.parse::<HotKey>().is_ok());
        assert_eq!(hotkey_label(DEFAULT_HOTKEY), "Ctrl+Alt+A");
        assert!("ctrl+shift+Space".parse::<HotKey>().is_ok());
        assert!("not a key".parse::<HotKey>().is_err());
    }

    #[test]
    fn icon_is_rgba_64() {
        assert_eq!(dot_rgba(GREEN).len(), 64 * 64 * 4);
    }
}
