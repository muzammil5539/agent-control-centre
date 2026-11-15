//! Local API token. The HUD creates it once; `acc` reads it from the data directory.
//! It stops other local users and web pages (which can POST to 127.0.0.1) from talking to the API.
use crate::config::data_dir;
use std::{io::Write, path::PathBuf};

pub fn token_path() -> Option<PathBuf> {
    Some(data_dir()?.join("token"))
}

/// Read the token written by the HUD. None when the HUD never ran on this account.
pub fn read() -> Option<String> {
    let t = std::fs::read_to_string(token_path()?).ok()?;
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

fn random_hex() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("OS random source unavailable");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The existing token, or a fresh one written with owner-only permissions.
pub fn load_or_create() -> String {
    if let Some(t) = read() {
        return t;
    }
    let t = random_hex();
    if let Some(p) = token_path() {
        let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        match o.open(&p).and_then(|mut f| f.write_all(t.as_bytes())) {
            Ok(()) => {}
            Err(e) => eprintln!("cannot save API token to {}: {e}", p.display()),
        }
    }
    t
}

/// Constant-time check of an `Authorization: Bearer <token>` header value.
pub fn check(header: Option<&str>, token: &str) -> bool {
    let given = header
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("")
        .trim();
    let (a, b) = (given.as_bytes(), token.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for (i, x) in b.iter().enumerate() {
        diff |= a.get(i).copied().unwrap_or(0) ^ x;
    }
    diff == 0 && !b.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_check() {
        let t = "abc123";
        assert!(check(Some("Bearer abc123"), t));
        assert!(!check(Some("Bearer abc124"), t));
        assert!(!check(Some("Bearer abc12"), t));
        assert!(!check(Some("abc123"), t)); // scheme required
        assert!(!check(None, t));
        assert!(!check(Some("Bearer "), "")); // an empty token never matches
    }

    #[test]
    fn tokens_are_random_and_long() {
        let (a, b) = (random_hex(), random_hex());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }
}
