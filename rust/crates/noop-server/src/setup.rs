//! First-time setup from the VPS's own shell, so a server works before any machine is connected:
//!
//!   noop-server 2fa-setup          bind the owner's authenticator app (shows a QR code, asks for one code)
//!   noop-server login-code         the pending sign-in code, when no SMTP is set up
//!   noop-server test-mail <to>     send a test mail with HUB_SMTP
//!
//! They read /etc/noop/noop.env themselves (run them as the `noop` user: `sudo -u noop noop-server …`).
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::db::{kv_get, kv_set};

/// The settings file the systemd unit uses.
pub const ENV_FILE: &str = "/etc/noop/noop.env";

/// Load KEY=VALUE lines into the environment, without overriding variables already set. Quotes around a value are
/// removed, as systemd's EnvironmentFile does.
pub fn load_env_file(path: &str) -> Result<usize, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let mut n = 0;
    for (k, v) in parse_env(&text) {
        if std::env::var_os(&k).is_none() {
            // SAFETY: called once at startup, before any other thread exists
            unsafe { std::env::set_var(&k, v) };
            n += 1;
        }
    }
    Ok(n)
}

pub fn parse_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            let v = v.trim();
            let v = if v.len() >= 2 && ((v.starts_with('\'') && v.ends_with('\'')) || (v.starts_with('"') && v.ends_with('"'))) { &v[1..v.len() - 1] } else { v };
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

/// The authenticator QR code for a terminal (two rows per character).
pub fn qr_text(uri: &str) -> String {
    match qrcode::QrCode::new(uri.as_bytes()) {
        Ok(code) => code
            .render::<qrcode::render::unicode::Dense1x2>()
            .dark_color(qrcode::render::unicode::Dense1x2::Light)
            .light_color(qrcode::render::unicode::Dense1x2::Dark)
            .quiet_zone(true)
            .build(),
        Err(_) => String::new(),
    }
}

/// Bind `secret` as the owner's authenticator once `code` matches it. Returns whether one was replaced.
pub fn bind_totp(c: &Connection, secret: &str, code: &str, now_ms: i64) -> Result<bool, String> {
    let step = noop_core::totp::totp_match(secret, code, -1, now_ms).ok_or("that code does not match: check the phone's clock and type the newest code")?;
    let was = kv_get(c, "totp").is_some_and(|t| t.get("secret").and_then(Value::as_str).is_some_and(|s| !s.is_empty()));
    kv_set(c, "totp", &json!({ "secret": secret, "last": step, "bound": now_ms })).map_err(|e| e.to_string())?;
    Ok(was)
}

/// The pending sign-in code (`noop login` without SMTP): (email, code), when one was requested in the last 10 minutes.
pub fn pending_login_code(c: &Connection, now_ms: i64) -> Option<(String, String)> {
    let v = kv_get(c, "cli_code")?;
    if v.get("expires").and_then(Value::as_i64).unwrap_or(0) <= now_ms {
        return None;
    }
    Some((v.get("email")?.as_str()?.to_string(), v.get("code")?.as_str()?.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file() {
        let v = parse_env("# c\nA=1\nB='{\"x\":1}'\n\nC=\"two words\"\nD=\n");
        assert_eq!(v, [("A".into(), "1".into()), ("B".into(), "{\"x\":1}".into()), ("C".into(), "two words".into()), ("D".into(), String::new())]);
    }

    #[test]
    fn binding_and_codes() {
        let c = Connection::open_in_memory().unwrap();
        crate::db::init(&c).unwrap();
        let secret = noop_core::totp::new_secret();
        let now = 1_780_000_000_000;
        assert!(bind_totp(&c, &secret, "000000", now).is_err() || noop_core::totp::totp_match(&secret, "000000", -1, now).is_some());
        let key = noop_core::totp::b32decode(&secret).unwrap();
        let code = noop_core::totp::hotp(&key, noop_core::totp::step_at(now) as u64);
        assert_eq!(bind_totp(&c, &secret, &code, now), Ok(false));
        assert_eq!(kv_get(&c, "totp").unwrap()["secret"], secret.as_str());
        assert_eq!(bind_totp(&c, &secret, &code, now), Ok(true), "re-binding reports the replacement");
        assert!(pending_login_code(&c, now).is_none());
        kv_set(&c, "cli_code", &json!({ "email": "a@b.c", "code": "ABCDEFGH", "expires": now + 1 })).unwrap();
        assert_eq!(pending_login_code(&c, now), Some(("a@b.c".into(), "ABCDEFGH".into())));
        assert!(pending_login_code(&c, now + 2).is_none());
        assert!(qr_text("otpauth://totp/x?secret=ABC").contains('█'));
    }
}
