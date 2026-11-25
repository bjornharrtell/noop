//! The team's shared secrets (Settings → MCP → Keys): API keys the owner enters once and every machine uses. Port of
//! cloud/src/secrets.ts and its routes:
//!
//!   GET /api/secrets          (owner or machine) → {secrets: [{name, updated}]}   never a value
//!   PUT /api/secrets          (owner) {name, value}  — value null or "" deletes
//!   GET /api/secrets/values   (machine) → {values: {NAME: value, NOOP_OAUTH_<SERVER>: access token}}
//!
//! Values are AES-256-GCM sealed with a key derived from HUB_TOKEN, the name bound as associated data, in the exact
//! format the Worker used ("b64(iv).b64(ciphertext)"), so an exported Worker database opens here.
use std::sync::{Arc, LazyLock};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::Aes256Gcm;
use axum::http::Method;
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use regex::Regex;
use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::app::{err, ok, App, Req};
use crate::auth::Who;

pub static SECRET_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]{0,63}$").unwrap());
pub const SECRET_MAX: usize = 8192;

fn cipher(token: &str) -> Aes256Gcm {
    let key = Sha256::digest(format!("noop-secrets:v1:{token}").as_bytes());
    Aes256Gcm::new_from_slice(&key).expect("32-byte key")
}

/// "iv.ciphertext", both base64.
pub fn seal(token: &str, name: &str, value: &str) -> String {
    let mut iv = [0u8; 12];
    noop_core::random_bytes(&mut iv);
    let ct = cipher(token)
        .encrypt(&iv.into(), Payload { msg: value.as_bytes(), aad: name.as_bytes() })
        .expect("AES-GCM encrypts any input");
    format!("{}.{}", STANDARD.encode(iv), STANDARD.encode(ct))
}

/// None when it does not open (the machine key changed since it was saved, or the data was altered).
pub fn unseal(token: &str, name: &str, sealed: &str) -> Option<String> {
    let (iv, ct) = sealed.split_once('.')?;
    let iv: [u8; 12] = STANDARD.decode(iv).ok()?.try_into().ok()?;
    let ct = STANDARD.decode(ct).ok()?;
    let pt = cipher(token).decrypt(&iv.into(), Payload { msg: &ct, aad: name.as_bytes() }).ok()?;
    String::from_utf8(pt).ok()
}

/// What is wrong with a PUT body, or None.
pub fn secret_problem(name: Option<&Value>, value: Option<&Value>) -> Option<&'static str> {
    if !name.and_then(Value::as_str).is_some_and(|n| SECRET_NAME.is_match(n)) {
        return Some("name: letters, digits and _, not starting with a digit");
    }
    match value {
        Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) if s.encode_utf16().count() > SECRET_MAX => Some("value too long"),
        Some(Value::String(_)) => None,
        _ => Some("value must be a string"),
    }
}

fn list(c: &Connection) -> rusqlite::Result<Vec<Value>> {
    c.prepare_cached("SELECT name, updated FROM secrets ORDER BY name")?
        .query_map([], |r| Ok(json!({ "name": r.get::<_, String>(0)?, "updated": r.get::<_, Option<String>>(1)? })))?
        .collect()
}

/// /api/secrets*; None when the path is not one of them.
pub async fn routes(app: &Arc<App>, req: &Req, w: &Who) -> rusqlite::Result<Option<Response>> {
    let machine = *w == Who::Machine;
    match (req.method.clone(), req.path.as_str()) {
        (Method::GET, "/api/secrets") => Ok(Some(ok(json!({ "secrets": list(&app.db())? })))),
        (Method::PUT, "/api/secrets") => {
            if machine {
                return Ok(Some(err("forbidden", 403)));
            }
            let b = req.json().unwrap_or_else(|| json!({}));
            if let Some(problem) = secret_problem(b.get("name"), b.get("value")) {
                return Ok(Some(err(problem, 400)));
            }
            let name = b["name"].as_str().unwrap_or_default();
            let c = app.db();
            match b.get("value").and_then(Value::as_str).filter(|v| !v.is_empty()) {
                None => c.execute("DELETE FROM secrets WHERE name = ?", [name])?,
                Some(v) => c.execute(
                    "INSERT OR REPLACE INTO secrets (name, sealed, updated) VALUES (?, ?, ?)",
                    params![name, seal(&app.cfg.hub_token, name, v), app.iso(app.now())],
                )?,
            };
            Ok(Some(ok(json!({ "secrets": list(&c)? }))))
        }
        (Method::GET, "/api/secrets/values") => {
            if !machine {
                return Ok(Some(err("forbidden", 403)));
            }
            let mut out = Map::new();
            {
                let c = app.db();
                let rows: Vec<(String, String)> = c
                    .prepare_cached("SELECT name, sealed FROM secrets")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                for (name, sealed) in rows {
                    if let Some(v) = unseal(&app.cfg.hub_token, &name, &sealed) {
                        out.insert(name, v.into());
                    }
                }
            }
            for name in crate::mcp_oauth::names(app)? {
                if let Some(t) = crate::mcp_oauth::token(app, &name).await? {
                    out.insert(crate::mcp_oauth::oauth_var(&name), t.into());
                }
            }
            Ok(Some(([("cache-control", "no-store")], ok(json!({ "values": out }))).into_response()))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sealing() {
        let s = seal("tok", "GITHUB_TOKEN", "ghp_x");
        assert_eq!(unseal("tok", "GITHUB_TOKEN", &s).as_deref(), Some("ghp_x"));
        assert_eq!(unseal("other", "GITHUB_TOKEN", &s), None, "another machine key");
        assert_eq!(unseal("tok", "OTHER_NAME", &s), None, "moved to another name");
        assert!(secret_problem(Some(&json!("1BAD")), Some(&json!("v"))).is_some());
        assert!(secret_problem(Some(&json!("OK")), Some(&Value::Null)).is_none());
        assert!(secret_problem(Some(&json!("OK")), Some(&json!(5))).is_some());
    }
    #[test]
    fn opens_a_value_sealed_elsewhere() {
        // sealed independently (Python `cryptography`, AESGCM) the way cloud/src/secrets.ts does with WebCrypto:
        // key = SHA-256("noop-secrets:v1:test"), iv = 00..0b, aad = "API_KEY", value "s3cret"
        assert_eq!(unseal("test", "API_KEY", "AAECAwQFBgcICQoL.RUAZh0RuUKYhUxNKRJxD/kksqigZlg==").as_deref(), Some("s3cret"));
    }
}
