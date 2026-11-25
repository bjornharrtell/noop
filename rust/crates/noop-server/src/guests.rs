//! Guest workers: low-trust machines (shared / public test devices) that get one task at a time and nothing else.
//! Port of cloud/src/guests.ts. A guest never holds HUB_TOKEN: it joins with a one-time code and gets its own
//! revocable token, and its results are untrusted (the primary reviews them; nothing is applied automatically).
//!
//!   Owner or machine: POST /api/guests/code {label} → {code, expires} · GET /api/guests · POST /api/guests/revoke {id}
//!   Anyone:           POST /api/guest/join {code} → {id, token}        (the one-time code is the credential)
//!   Machine:          POST /api/guests/assign {guest, task} · GET /api/guests/results · POST /api/guests/ack {ids}
//!   Guest token:      GET /api/guest/next · POST /api/guest/result {id, ok, text, patch?} · PUT /api/guest/heartbeat
use std::sync::LazyLock;

use axum::http::Method;
use axum::response::Response;
use regex::Regex;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

use crate::app::{err, ok, sha256hex, App, Req};
use crate::auth::Who;

pub static GUEST_TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^g_[0-9a-f]{64}$").unwrap());
static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{32}$").unwrap());
static TASK_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^T-[A-Za-z0-9_-]{1,40}$").unwrap());
pub const CODE_TTL: i64 = 10 * 60_000;
/// text + patch
pub const MAX_RESULT: usize = 256 * 1024;
pub const MAX_TASK: usize = 32 * 1024;
const MAX_QUEUED: i64 = 20;
/// A claimed task with no result goes back to the queue.
const CLAIM_TIMEOUT: i64 = 6 * 3_600_000;

fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    noop_core::random_bytes(&mut b);
    hex::encode(b)
}

/// No control characters except tab and newline; at most `n` characters.
fn clean(v: Option<&Value>, n: usize) -> String {
    let s = match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    s.chars()
        .filter(|&c| !matches!(c as u32, 0x00..=0x08 | 0x0b..=0x1f | 0x7f..=0x9f))
        .take(n)
        .collect()
}

/// The guest behind a token, or None (unknown or revoked).
pub fn who(app: &App, token: &str) -> rusqlite::Result<Option<String>> {
    if !GUEST_TOKEN.is_match(token) {
        return Ok(None);
    }
    let c = app.db();
    let r: Option<(String, i64, i64)> = c
        .query_row("SELECT id, revoked, last_seen FROM guests WHERE token_hash = ?", [sha256hex(token.as_bytes())], |r| {
            Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0), r.get::<_, Option<i64>>(2)?.unwrap_or(0)))
        })
        .optional()?;
    let Some((id, _, last_seen)) = r.filter(|r| r.1 == 0) else { return Ok(None) };
    if app.now() - last_seen > 60_000 {
        c.execute("UPDATE guests SET last_seen = ? WHERE id = ?", params![app.now(), id])?;
    }
    Ok(Some(id))
}

/// Unauthenticated: trade a one-time code for a guest token. Codes are single use and expire in 10 minutes.
pub fn join(app: &App, req: &Req) -> rusqlite::Result<Response> {
    let code = req.json().and_then(|b| b.get("code").map(|c| c.as_str().map(String::from).unwrap_or_else(|| c.to_string()))).unwrap_or_default();
    let code = code.trim().to_lowercase();
    if !CODE.is_match(&code) {
        return Ok(err("invalid code", 400));
    }
    let h = sha256hex(code.as_bytes());
    let now = app.now();
    let c = app.db();
    let row: Option<(String, i64)> = c.query_row("SELECT label, expires FROM guest_codes WHERE hash = ?", [&h], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    c.execute("DELETE FROM guest_codes WHERE hash = ? OR expires < ?", params![h, now])?;
    let Some((label, _)) = row.filter(|r| r.1 >= now) else { return Ok(err("invalid code", 401)) };
    let (id, token) = (format!("G-{}", rand_hex(4)), format!("g_{}", rand_hex(32)));
    c.execute(
        "INSERT INTO guests (id, label, token_hash, created, last_seen, revoked) VALUES (?, ?, ?, ?, ?, 0)",
        params![id, label, sha256hex(token.as_bytes()), now, now],
    )?;
    let text = app.l(
        &format!("Guest machine {label} ({id}) joined. It can only take tasks assigned to it."),
        &format!("访客机器 {label}（{id}）已加入，只能领取派给它的任务。"),
    );
    crate::db::append(app, &c, json!({ "from": "hub", "kind": "notice", "text": text }));
    Ok(ok(json!({ "id": id, "token": token })))
}

/// Everything a guest token may do.
pub fn api(app: &App, req: &Req, guest: &str) -> rusqlite::Result<Response> {
    let now = app.now();
    let c = app.db();
    match (req.method.clone(), req.path.as_str()) {
        (Method::PUT, "/api/guest/heartbeat") => {
            c.execute("UPDATE guests SET last_seen = ? WHERE id = ?", params![now, guest])?;
            Ok(ok(json!({ "ok": true })))
        }
        (Method::GET, "/api/guest/next") => {
            c.execute(
                "UPDATE guest_tasks SET state = 'queued', claimed = NULL WHERE guest = ? AND state = 'claimed' AND claimed < ?",
                params![guest, now - CLAIM_TIMEOUT],
            )?;
            let r: Option<(String, String)> =
                c.query_row("SELECT id, task FROM guest_tasks WHERE guest = ? AND state = 'queued' ORDER BY ts LIMIT 1", [guest], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let Some((id, task)) = r else { return Ok(ok(json!({ "task": null }))) };
            c.execute("UPDATE guest_tasks SET state = 'claimed', claimed = ? WHERE id = ?", params![now, id])?;
            Ok(ok(json!({ "task": serde_json::from_str::<Value>(&task).unwrap_or(Value::Null) })))
        }
        (Method::POST, "/api/guest/result") => {
            if req.body.len() > MAX_RESULT {
                return Ok(err("result too large", 413));
            }
            let Some(b) = req.json() else { return Ok(err("bad json", 400)) };
            let id = clean(b.get("id"), 200);
            let state: Option<String> = c.query_row("SELECT state FROM guest_tasks WHERE id = ? AND guest = ?", params![id, guest], |r| r.get(0)).optional()?;
            if state.as_deref() != Some("claimed") {
                return Ok(err("not your task", 403));
            }
            let mut result = json!({ "ok": b.get("ok") == Some(&Value::Bool(true)), "text": clean(b.get("text"), MAX_RESULT) });
            if b.get("patch").is_some_and(Value::is_string) {
                result["patch"] = clean(b.get("patch"), MAX_RESULT).into();
            }
            c.execute("UPDATE guest_tasks SET state = 'done', result = ? WHERE id = ?", params![result.to_string(), id])?;
            let text = app.l(&format!("Guest machine {guest} returned the result of {id} (not reviewed)."), &format!("访客机器 {guest} 交回了 {id} 的结果（未经审查）。"));
            crate::db::append(app, &c, json!({ "from": "hub", "kind": "notice", "ref": id, "text": text }));
            Ok(ok(json!({ "ok": true })))
        }
        _ => Ok(err("forbidden", 403)), // nothing else exists for a guest
    }
}

/// Owner-session and machine routes under /api/guests*; None when the path isn't one of them.
pub fn admin(app: &App, req: &Req, w: &Who) -> rusqlite::Result<Option<Response>> {
    let p = req.path.as_str();
    if !p.starts_with("/api/guests") {
        return Ok(None);
    }
    let machine = *w == Who::Machine;
    let now = app.now();
    let c = app.db();
    let m = req.method.clone();
    if p == "/api/guests" && m == Method::GET {
        let guests: Vec<Value> = c
            .prepare_cached("SELECT id, label, created, last_seen, revoked FROM guests ORDER BY created")?
            .query_map([], |r| {
                Ok(json!({ "id": r.get::<_, String>(0)?, "label": r.get::<_, Option<String>>(1)?, "created": r.get::<_, Option<i64>>(2)?,
                    "last_seen": r.get::<_, Option<i64>>(3)?, "revoked": r.get::<_, Option<i64>>(4)?.unwrap_or(0) != 0 }))
            })?
            .collect::<rusqlite::Result<_>>()?;
        return Ok(Some(ok(json!({ "guests": guests }))));
    }
    if p == "/api/guests/code" && m == Method::POST {
        // owner (app) or a machine (`noop guest invite`): both already outrank a guest
        let label = clean(req.json().as_ref().and_then(|b| b.get("label")), 60).trim().to_string();
        if label.is_empty() {
            return Ok(Some(err("label required", 400)));
        }
        let (code, expires) = (rand_hex(16), now + CODE_TTL);
        c.execute("INSERT INTO guest_codes (hash, label, expires) VALUES (?, ?, ?)", params![sha256hex(code.as_bytes()), label, expires])?;
        return Ok(Some(ok(json!({ "code": code, "expires": expires }))));
    }
    if p == "/api/guests/revoke" && m == Method::POST {
        let id = clean(req.json().as_ref().and_then(|b| b.get("id")), 200);
        c.execute("UPDATE guests SET revoked = 1 WHERE id = ?", [&id])?;
        c.execute("DELETE FROM guest_tasks WHERE guest = ? AND state != 'done'", [&id])?;
        return Ok(Some(ok(json!({ "ok": true }))));
    }
    if !machine {
        return Ok(Some(err("forbidden", 403)));
    }
    if p == "/api/guests/assign" && m == Method::POST {
        if req.body.len() > MAX_TASK {
            return Ok(Some(err("task too large", 413)));
        }
        let b = if req.body.is_empty() { json!({}) } else {
            match req.json() {
                Some(b) => b,
                None => return Ok(Some(err("bad json", 400))),
            }
        };
        let guest = clean(b.get("guest"), 200);
        let g: Option<String> = c.query_row("SELECT id FROM guests WHERE id = ? AND revoked = 0", [&guest], |r| r.get(0)).optional()?;
        let Some(g) = g else { return Ok(Some(err("no such guest", 404))) };
        let task = b.get("task").cloned().unwrap_or(Value::Null);
        let id = clean(task.get("id"), 200);
        if !TASK_ID.is_match(&id) {
            return Ok(Some(err("task id required", 400)));
        }
        let repo = task.get("repo").filter(|r| !matches!(r, Value::Null | Value::Bool(false)) && r.as_str() != Some(""));
        if let Some(r) = repo {
            let s = r.as_str().unwrap_or("");
            if !(s.starts_with("https://") && s.len() > 8 && !s.chars().any(char::is_whitespace)) {
                return Ok(Some(err("repo must be an https:// URL", 400)));
            }
        }
        let n: i64 = c.query_row("SELECT COUNT(*) FROM guest_tasks WHERE guest = ? AND state != 'done'", [&g], |r| r.get(0))?;
        if n >= MAX_QUEUED {
            return Ok(Some(err("queue full", 429)));
        }
        // only what the guest needs: the task text the primary wrote for it, never memory or the owner's profile
        let mut t = json!({ "id": id, "title": clean(task.get("title"), 200), "body": clean(task.get("body"), MAX_TASK) });
        if let Some(r) = repo {
            t["repo"] = clean(Some(r), 200).into();
        }
        c.execute(
            "INSERT OR REPLACE INTO guest_tasks (id, guest, task, state, claimed, result, ts) VALUES (?, ?, ?, 'queued', NULL, NULL, ?)",
            params![id, g, t.to_string(), now],
        )?;
        return Ok(Some(ok(json!({ "ok": true }))));
    }
    if p == "/api/guests/results" && m == Method::GET {
        let results: Vec<Value> = c
            .prepare_cached("SELECT t.id, t.guest, g.label, t.result FROM guest_tasks t JOIN guests g ON g.id = t.guest WHERE t.state = 'done' ORDER BY t.ts LIMIT 20")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?)))?
            .filter_map(Result::ok)
            .map(|(id, guest, label, result)| {
                let mut v = json!({ "id": id, "guest": guest, "label": label, "untrusted": true });
                if let Some(Value::Object(m)) = result.and_then(|r| serde_json::from_str(&r).ok()) {
                    v.as_object_mut().expect("object").extend(m);
                }
                v
            })
            .collect();
        return Ok(Some(ok(json!({ "results": results }))));
    }
    if p == "/api/guests/ack" && m == Method::POST {
        let ids = req.json().and_then(|b| b.get("ids").and_then(Value::as_array).cloned()).unwrap_or_default();
        for id in ids.iter().take(50) {
            c.execute("DELETE FROM guest_tasks WHERE id = ? AND state = 'done'", [clean(Some(id), 200)])?;
        }
        return Ok(Some(ok(json!({ "ok": true }))));
    }
    Ok(Some(err("not found", 404)))
}
