//! The chat feed, the owner→fleet inbox, the dashboard and settings, tasks and channels. Port of those routes in
//! cloud/src/index.ts.
//!
//!   Owner:  GET /api/feed?since= · POST /api/messages · GET /api/status · GET|PUT /api/settings · POST /api/fleet/update
//!           GET /api/tasks · GET /api/tasks/get?id= · POST /api/tasks/cancel · GET|POST /api/channels · GET /api/channels/feed
//!   Fleet:  POST /api/events · PUT /api/host/state · GET /api/host/inbox?after=&wait=
use std::sync::Arc;
use std::time::Duration;

use axum::http::Method;
use axum::response::Response;
use noop_core::tools::{self, FileRow, Store, CANCEL_ID, CHANNEL_SLUG};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};

use crate::app::{err, js_num, ok, App, Req};
use crate::auth::Who;
use crate::db::{append, kv_get, kv_set};

/// A host counts as online this long after it was last heard from.
const STALE_MS: i64 = 3 * 60_000;
const MAX_ATTACHMENTS: usize = 10;

/// The tool views' window on the files table (the Worker's `listDir` / `readFile`).
pub struct DbStore<'a>(pub &'a Connection);

impl Store for DbStore<'_> {
    fn read(&self, path: &str) -> Option<FileRow> {
        self.0
            .query_row("SELECT content, updated FROM files WHERE path = ? AND deleted = 0", [path], |r| {
                Ok(FileRow {
                    path: path.to_string(),
                    text: String::from_utf8_lossy(&r.get::<_, Option<Vec<u8>>>(0)?.unwrap_or_default()).into_owned(),
                    updated: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                })
            })
            .optional()
            .ok()
            .flatten()
    }
    fn list(&self, dir: &str) -> Vec<FileRow> {
        let end = noop_core::paths::prefix_end(dir);
        let Ok(mut st) = self.0.prepare_cached(
            "SELECT path, content, updated FROM files WHERE path >= ? AND path < ? AND deleted = 0 ORDER BY path DESC LIMIT 2000",
        ) else {
            return Vec::new();
        };
        st.query_map(params![dir, end], |r| {
            Ok(FileRow {
                path: r.get(0)?,
                text: String::from_utf8_lossy(&r.get::<_, Option<Vec<u8>>>(1)?.unwrap_or_default()).into_owned(),
                updated: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }
}

pub fn host_online(app: &App, c: &Connection) -> bool {
    if app.host_sockets() > 0 {
        return true;
    }
    kv_get(c, "host_seen").and_then(|v| v.as_i64()).is_some_and(|seen| app.now() - seen < STALE_MS)
}

pub fn touch_host(app: &App, c: &Connection) -> rusqlite::Result<()> {
    kv_set(c, "host_seen", &json!(app.now()))
}

/// The dashboard: what the primary last sent, plus whether a host is online.
pub fn status_view(app: &App, c: &Connection) -> Value {
    let mut s = match kv_get(c, "status") {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    s.insert("hub".into(), json!({ "online": host_online(app, c), "seen": kv_get(c, "host_seen") }));
    Value::Object(s)
}

/// Owner→fleet inbox. A waiting long-poll (and, once ported, the primary's socket) is woken at once.
pub fn to_inbox(app: &App, c: &Connection, item: Value) -> rusqlite::Result<()> {
    c.execute("INSERT INTO inbox (json) VALUES (?)", [item.to_string()])?;
    app.inbox_notify.notify_waiters();
    let _ = app.host_tx.send(Arc::from(r#"{"type":"inbox"}"#));
    Ok(())
}

/// What the owner sent: text plus where it belongs.
#[derive(Default)]
pub struct OwnerMsg<'a> {
    pub text: &'a str,
    pub reply_to: Option<&'a str>,
    pub attachments: &'a [String],
    /// Said in a task's thread (task page), not the main chat.
    pub thread: bool,
    /// The client's idempotency key: a retried send returns the stored event.
    pub client_id: Option<&'a str>,
    pub channel: Option<&'a str>,
}

pub fn owner_message(app: &App, c: &Connection, msg: OwnerMsg) -> rusqlite::Result<Value> {
    let OwnerMsg { text, reply_to, attachments, thread, client_id, channel } = msg;
    if let Some(id) = client_id {
        let prev: Option<String> =
            c.query_row("SELECT json FROM events WHERE json_extract(json, '$.client_id') = ? LIMIT 1", [id], |r| r.get(0)).optional()?;
        if let Some(p) = prev.and_then(|p| serde_json::from_str(&p).ok()) {
            return Ok(p);
        }
    }
    let mut e = Map::new();
    e.insert("from".into(), "owner".into());
    e.insert("kind".into(), "owner".into());
    e.insert("text".into(), text.into());
    if let Some(r) = reply_to {
        e.insert("ref".into(), r.into());
    }
    if let Some(id) = client_id {
        e.insert("client_id".into(), id.into());
    }
    if !attachments.is_empty() {
        e.insert("attachments".into(), attachments.into());
    }
    if thread {
        e.insert("thread".into(), true.into());
    }
    if let Some(ch) = channel {
        e.insert("channel".into(), ch.into());
    }
    let ev = append(app, c, Value::Object(e));
    to_inbox(app, c, json!({ "type": "message", "event": ev }))?;
    if !host_online(app, c) {
        let mut note = json!({ "from": "hub", "kind": "system", "ref": ev["id"],
            "text": app.l("Got it. The team's machines are all offline right now, this will be handled as soon as one is back.", "收到。团队的电脑暂时都不在线，上线后会马上处理。") });
        if let Some(ch) = channel {
            note["channel"] = ch.into();
        }
        append(app, c, note);
    }
    Ok(ev)
}

/// A message's attachments: up to 10 of the hub's own media paths ("/media/<id>"); None when malformed.
fn owner_attachments(v: Option<&Value>) -> Option<Vec<String>> {
    match v {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(a)) if a.len() <= MAX_ATTACHMENTS => a
            .iter()
            .map(|x| {
                let s = x.as_str()?;
                let id = s.strip_prefix("/media/")?;
                let ok = !s.contains("..")
                    && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                    && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-');
                ok.then(|| s.to_string())
            })
            .collect(),
        _ => None,
    }
}

/// Media older than settings `media_days` (7, 14, 30 or 90; 7 by default) goes, checked at most hourly. Ids start
/// with the upload time in ms.
pub fn prune_media(app: &App, c: &Connection, force: bool) -> rusqlite::Result<()> {
    let now = app.now();
    let last = kv_get(c, "media_pruned").and_then(|v| v.as_i64()).unwrap_or(0);
    if !force && now - last < 3_600_000 {
        return Ok(());
    }
    kv_set(c, "media_pruned", &json!(now))?;
    let s = kv_get(c, "settings").unwrap_or(Value::Null);
    let want = s.pointer("/current/media_days").or(s.pointer("/effective/media_days")).and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()));
    let days = match want {
        Some(d) if [7.0, 14.0, 30.0, 90.0].contains(&d) => d as i64,
        _ => 7,
    };
    c.execute(
        "DELETE FROM media WHERE instr(id, '-') = 14 AND CAST(substr(id, 1, 13) AS INTEGER) < ?",
        [now - days * 86_400_000],
    )?;
    Ok(())
}

fn deep_merge(a: Value, b: &Value) -> Value {
    let mut o = match a {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    if let Value::Object(bm) = b {
        for (k, v) in bm {
            let merged = if v.is_object() { deep_merge(o.remove(k).unwrap_or(Value::Null), v) } else { v.clone() };
            o.insert(k.clone(), merged);
        }
    }
    Value::Object(o)
}

fn events_json(rows: Vec<String>) -> Vec<Value> {
    rows.into_iter().filter_map(|s| serde_json::from_str(&s).ok()).collect()
}

/// The synchronous feed routes. `None` when the path is not one of them.
pub fn routes(app: &Arc<App>, c: &Connection, req: &Req, w: &Who) -> rusqlite::Result<Option<Response>> {
    let p = req.path.as_str();
    let m = &req.method;
    let machine = *w == Who::Machine;
    let r = match (m.clone(), p) {
        (_, "/api/feed") => {
            let since = req.q("since").unwrap_or("");
            let mut st = c.prepare_cached("SELECT json FROM (SELECT seq, json FROM events WHERE ts > ? ORDER BY ts DESC LIMIT 300) ORDER BY seq")?;
            let rows = st.query_map([since], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            ok(json!({ "events": events_json(rows) }))
        }
        (Method::POST, "/api/messages") => {
            let Some(b) = req.json() else { return Ok(Some(err("bad json", 400))) };
            let text = b.get("text").and_then(Value::as_str).unwrap_or("");
            let room = match b.get("channel") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if s.is_empty() => None,
                Some(ch) => {
                    let slug = ch.as_str().unwrap_or("");
                    let view = tools::list_channel_rows(&DbStore(c));
                    match view.into_iter().find(|r| r.slug == slug) {
                        Some(r) => Some(r.slug),
                        None => return Ok(Some(err("channel not found", 400))),
                    }
                }
            };
            let client_id = match b.get("client_id") {
                None => None,
                Some(Value::String(s)) if (1..=100).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || "_:-".contains(c)) => Some(s.as_str()),
                Some(_) => return Ok(Some(err("bad client_id", 400))),
            };
            let Some(files) = owner_attachments(b.get("attachments")) else { return Ok(Some(err("bad attachments", 400))) };
            for f in &files {
                let found: Option<i64> = c.query_row("SELECT 1 FROM media WHERE id = ? LIMIT 1", [&f[7..]], |r| r.get(0)).optional()?;
                if found.is_none() {
                    return Ok(Some(err("attachment not found", 400)));
                }
            }
            if text.trim().is_empty() && files.is_empty() {
                return Ok(Some(err("text required", 400)));
            }
            let reply_to = b.get("reply_to").and_then(Value::as_str);
            let thread = b.get("thread") == Some(&Value::Bool(true)) && reply_to.is_some_and(|r| r.starts_with("T-"));
            ok(owner_message(app, c, OwnerMsg { text, reply_to, attachments: &files, thread, client_id, channel: room.as_deref() })?)
        }
        (_, "/api/status") => ok(status_view(app, c)),
        (Method::GET, "/api/settings") => ok(kv_get(c, "settings").unwrap_or_else(|| json!({ "current": {}, "effective": {}, "options": {} }))),
        (Method::PUT, "/api/settings") => {
            let Some(patch) = req.json() else { return Ok(Some(err("bad json", 400))) };
            to_inbox(app, c, json!({ "type": "settings", "patch": patch }))?;
            let mut view = kv_get(c, "settings").unwrap_or_else(|| json!({ "current": {} }));
            let cur = view.get("current").cloned().unwrap_or_else(|| json!({}));
            view["current"] = deep_merge(cur, &patch);
            kv_set(c, "settings", &view)?;
            if patch.get("media_days").is_some() {
                prune_media(app, c, true)?;
            }
            ok(view)
        }
        (Method::POST, "/api/fleet/update") => {
            if machine {
                return Ok(Some(err("forbidden", 403)));
            }
            to_inbox(app, c, json!({ "type": "update" }))?;
            ok(json!({ "ok": true }))
        }
        (Method::GET, "/api/tasks") => {
            ok(tools::task_history(&DbStore(c), req.q("q").unwrap_or(""), js_num(req.q("offset")), js_num(req.q("limit"))))
        }
        (Method::GET, "/api/tasks/get") => {
            let Some(mut t) = tools::task_info(&DbStore(c), req.q("id").unwrap_or("")) else { return Ok(Some(err("not found", 404))) };
            // its whole conversation, however far back: the app's feed only holds the newest events
            let mut refs: Vec<String> = vec![t["id"].as_str().unwrap_or("").to_string()];
            refs.extend(t["decisions"].as_array().into_iter().flatten().filter_map(|d| d.as_str().map(String::from)));
            let mut st = c.prepare_cached("SELECT json FROM events WHERE ref IN (SELECT value FROM json_each(?)) ORDER BY seq")?;
            let rows = st.query_map([serde_json::to_string(&refs).unwrap_or_default()], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            t.insert("events".into(), events_json(rows).into());
            ok(Value::Object(t))
        }
        (Method::POST, "/api/tasks/cancel") => {
            if machine {
                return Ok(Some(err("forbidden", 403)));
            }
            let b = req.json().unwrap_or(Value::Null);
            let id = b.get("id").and_then(Value::as_str).unwrap_or("");
            if !CANCEL_ID.is_match(id) {
                return Ok(Some(err("id required", 400)));
            }
            to_inbox(app, c, json!({ "type": "cancel", "id": id, "done": b.get("done") == Some(&Value::Bool(true)) }))?;
            ok(json!({ "ok": true }))
        }
        (Method::GET, "/api/channels") => ok(tools::channels_view(&DbStore(c), app.now(), 100)),
        (Method::GET, "/api/channels/feed") => {
            let view = tools::channels_view(&DbStore(c), app.now(), 100);
            let slug = req.q("slug").unwrap_or("");
            let Some(ch) = view["channels"].as_array().into_iter().flatten().find(|ch| ch["slug"] == slug) else {
                return Ok(Some(err("not found", 404)));
            };
            let refs: Vec<String> = ch["refs"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(String::from)).collect();
            let mut st = c.prepare_cached(
                "SELECT json FROM events WHERE ref IN (SELECT value FROM json_each(?)) OR json_extract(json, '$.channel') = ? ORDER BY seq DESC LIMIT 5000",
            )?;
            let mut rows = st.query_map(params![serde_json::to_string(&refs).unwrap_or_default(), slug], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows.reverse();
            ok(json!({ "events": tools::channel_digest(events_json(rows), &refs, slug, 80) }))
        }
        (Method::POST, "/api/channels") => {
            if machine {
                return Ok(Some(err("forbidden", 403)));
            }
            let b = req.json().unwrap_or(Value::Null);
            let slug = b.get("slug").and_then(Value::as_str).unwrap_or("");
            if !CHANNEL_SLUG.is_match(slug) {
                return Ok(Some(err("slug required", 400)));
            }
            let mut item = json!({ "type": "channel", "slug": slug });
            if let Some(g) = b.get("goal").and_then(Value::as_str) {
                item["goal"] = crate::app::truncate(g, 500).into();
            }
            if let Some(a) = b.get("archived").and_then(Value::as_bool) {
                item["archived"] = a.into();
            }
            to_inbox(app, c, item)?;
            ok(json!({ "ok": true }))
        }
        // ---- fleet side
        (Method::POST, "/api/events") => {
            let Some(e) = req.json().filter(Value::is_object) else { return Ok(Some(err("bad json", 400))) };
            // the event id is the idempotency key: a host that never saw the reply posts the same event again
            if let Some(id) = e.get("id").and_then(Value::as_str) {
                let dup: Option<i64> = c.query_row("SELECT 1 FROM events WHERE json_extract(json, '$.id') = ? LIMIT 1", [id], |r| r.get(0)).optional()?;
                if dup.is_some() {
                    return Ok(Some(ok(json!({ "duplicate": true }))));
                }
            }
            ok(append(app, c, e))
        }
        (Method::PUT, "/api/host/state") => {
            let b = req.json().unwrap_or(Value::Null);
            if let Some(status) = b.get("status").filter(|s| !is_falsy(s)) {
                kv_set(c, "status", status)?;
                // TODO(rust-port): MCP resources/updated noop://status
            }
            if let Some(settings) = b.get("settings").filter(|s| !is_falsy(s)) {
                kv_set(c, "settings", settings)?;
            }
            touch_host(app, c)?;
            if b.get("status").is_some_and(|s| !is_falsy(s)) {
                crate::stream::push_status(app, c);
            }
            // watchers: owner clients connected right now. With none, the primary skips live-activity-only refreshes.
            ok(json!({ "ok": true, "watchers": app.owner_sockets(), "level": "ok" }))
        }
        _ => return Ok(None),
    };
    Ok(Some(r))
}

fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

/// GET /api/host/inbox?after=<seq>[&wait=<s>]: acknowledges everything up to `after`, then returns the next items. With
/// `wait` (≤ 25 s) an empty inbox holds the request until something arrives.
pub async fn inbox(app: &Arc<App>, req: &Req) -> rusqlite::Result<Response> {
    let after = js_num(req.q("after"));
    let after = if after.is_nan() { 0 } else { after as i64 };
    let wait = js_num(req.q("wait"));
    let wait = if wait.is_nan() { 0.0 } else { wait.clamp(0.0, 25.0) };
    let read = |c: &Connection| -> rusqlite::Result<Vec<Value>> {
        let mut st = c.prepare_cached("SELECT seq, json FROM inbox ORDER BY seq LIMIT 50")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .map(|(seq, j)| {
                let mut o = Map::new();
                o.insert("seq".into(), seq.into());
                if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&j) {
                    o.extend(m);
                }
                Value::Object(o)
            })
            .collect())
    };
    // registered before the read, so an item stored between the read and the wait still wakes this request
    let notified = app.inbox_notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let items = {
        let c = app.db();
        let mut acked = app.acked.lock().unwrap_or_else(|e| e.into_inner());
        if after > *acked {
            c.execute("DELETE FROM inbox WHERE seq <= ?", [after])?;
            *acked = after;
        }
        drop(acked);
        touch_host(app, &c)?;
        read(&c)?
    };
    if !items.is_empty() || wait == 0.0 {
        return Ok(ok(json!({ "items": items })));
    }
    let _ = tokio::time::timeout(Duration::from_secs_f64(wait), notified).await;
    let c = app.db();
    touch_host(app, &c)?;
    Ok(ok(json!({ "items": read(&c)? })))
}
