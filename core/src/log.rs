//! Structured log: one JSON object per line in `<data dir>/acc.log` (rotated at 1 MiB).
//! `ACC_LOG=stderr` mirrors it to the terminal.
use serde_json::{Value, json};
use std::{fs::File, io::Write, sync::Mutex};

static FILE: Mutex<Option<File>> = Mutex::new(None);
const MAX: u64 = 1 << 20;

pub fn log(level: &str, msg: &str, mut fields: Value) {
    if cfg!(test) {
        return; // unit tests must not write into the real data directory
    }
    if !fields.is_object() {
        fields = json!({});
    }
    fields["ts"] = crate::store::now_secs().into();
    fields["level"] = level.into();
    fields["msg"] = crate::store::redact(msg).into();
    let line = fields.to_string();
    if std::env::var("ACC_LOG").is_ok_and(|v| v == "stderr") {
        eprintln!("{line}");
    }
    let Some(path) = crate::config::data_dir().map(|d| d.join("acc.log")) else {
        return;
    };
    let mut f = FILE.lock().unwrap();
    if f.as_ref()
        .and_then(|f| f.metadata().ok())
        .is_some_and(|m| m.len() > MAX)
    {
        *f = None;
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    if f.is_none() {
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
        *f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
    }
    if let Some(f) = f.as_mut() {
        let _ = writeln!(f, "{line}");
    }
}

pub fn info(msg: &str, fields: Value) {
    log("info", msg, fields)
}
pub fn warn(msg: &str, fields: Value) {
    log("warn", msg, fields)
}
