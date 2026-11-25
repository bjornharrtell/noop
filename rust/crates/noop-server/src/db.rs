//! The database: the Worker's tables with the same names and columns, so `/api/host/export` from a Cloudflare
//! deployment imports as is. Columns the Worker added later with ALTER TABLE are part of CREATE TABLE here.
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{Map, Value};

use crate::app::App;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS events (seq INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT, json TEXT, ref TEXT);
CREATE INDEX IF NOT EXISTS events_ts ON events (ts);
CREATE INDEX IF NOT EXISTS events_ref ON events (ref);
CREATE INDEX IF NOT EXISTS events_channel ON events (json_extract(json, '$.channel')) WHERE json_extract(json, '$.channel') IS NOT NULL;
CREATE INDEX IF NOT EXISTS events_client_id ON events (json_extract(json, '$.client_id')) WHERE json_extract(json, '$.client_id') IS NOT NULL;
CREATE INDEX IF NOT EXISTS events_id ON events (json_extract(json, '$.id')) WHERE json_extract(json, '$.id') IS NOT NULL;
CREATE TABLE IF NOT EXISTS inbox (seq INTEGER PRIMARY KEY AUTOINCREMENT, json TEXT);
CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT);
CREATE TABLE IF NOT EXISTS media (id TEXT, n INTEGER, type TEXT, data BLOB, PRIMARY KEY (id, n));
CREATE TABLE IF NOT EXISTS auth_codes (email TEXT PRIMARY KEY, hash TEXT, expires INTEGER, attempts INTEGER);
CREATE TABLE IF NOT EXISTS auth_sends (email TEXT, ts INTEGER, sent INTEGER);
CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, email TEXT, created INTEGER, last_seen INTEGER, ua TEXT, scope TEXT);
CREATE TABLE IF NOT EXISTS push_subs (endpoint TEXT PRIMARY KEY, session TEXT, p256dh TEXT, auth TEXT, label TEXT, created INTEGER);
CREATE TABLE IF NOT EXISTS secrets (name TEXT PRIMARY KEY, sealed TEXT, updated TEXT);
CREATE TABLE IF NOT EXISTS mcp_oauth (name TEXT PRIMARY KEY, sealed TEXT, updated TEXT);
CREATE TABLE IF NOT EXISTS mcp_oauth_pending (state TEXT PRIMARY KEY, name TEXT, verifier TEXT, created INTEGER);
CREATE TABLE IF NOT EXISTS files (path TEXT PRIMARY KEY, content BLOB, hash TEXT, version INTEGER NOT NULL,
  updated TEXT, by TEXT, deleted INTEGER NOT NULL DEFAULT 0, rev INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS files_rev ON files (rev);
CREATE TABLE IF NOT EXISTS history (seq INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT, version INTEGER, hash TEXT,
  content BLOB, ts TEXT, by TEXT, msg TEXT);
CREATE INDEX IF NOT EXISTS history_path ON history (path, version);
CREATE TABLE IF NOT EXISTS files_meta (k TEXT PRIMARY KEY, v INTEGER);
CREATE TABLE IF NOT EXISTS guests (id TEXT PRIMARY KEY, label TEXT, token_hash TEXT UNIQUE, created INTEGER, last_seen INTEGER, revoked INTEGER);
CREATE TABLE IF NOT EXISTS guest_codes (hash TEXT PRIMARY KEY, label TEXT, expires INTEGER);
CREATE TABLE IF NOT EXISTS guest_tasks (id TEXT PRIMARY KEY, guest TEXT, task TEXT, state TEXT, claimed INTEGER, result TEXT, ts INTEGER);
CREATE INDEX IF NOT EXISTS guest_tasks_q ON guest_tasks (guest, state);
CREATE TABLE IF NOT EXISTS oauth_clients (id TEXT PRIMARY KEY, json TEXT, created INTEGER);
CREATE TABLE IF NOT EXISTS oauth_codes (hash TEXT PRIMARY KEY, client_id TEXT, redirect_uri TEXT, challenge TEXT, email TEXT, expires INTEGER);
CREATE TABLE IF NOT EXISTS oauth_refresh (hash TEXT PRIMARY KEY, client_id TEXT, email TEXT, session TEXT, created INTEGER);
CREATE TABLE IF NOT EXISTS mcp_sessions (id TEXT PRIMARY KEY, level TEXT, subs TEXT, created INTEGER, last INTEGER, version TEXT, g_seen INTEGER DEFAULT 0);
CREATE TABLE IF NOT EXISTS mcp_events (session TEXT, stream TEXT, seq INTEGER, data TEXT, ts INTEGER, PRIMARY KEY (session, stream, seq));
";

pub fn init(c: &Connection) -> rusqlite::Result<()> {
    // WAL: readers never block the writer; synchronous=NORMAL is durable across process crashes (not power loss of
    // the last few commits), the usual trade-off for a small server.
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "NORMAL")?;
    c.pragma_update(None, "busy_timeout", 5000)?;
    c.execute_batch(SCHEMA)
}

/// Database size in bytes (the Worker's `sql.databaseSize`).
pub fn size(c: &Connection) -> i64 {
    let pages: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap_or(0);
    let page: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap_or(0);
    pages * page
}

pub fn kv_get(c: &Connection, k: &str) -> Option<Value> {
    let v: Option<String> = c.query_row("SELECT v FROM kv WHERE k = ?", [k], |r| r.get(0)).optional().ok().flatten();
    v.and_then(|s| serde_json::from_str(&s).ok())
}

/// Writes only when the value changed.
pub fn kv_set(c: &Connection, k: &str, v: &Value) -> rusqlite::Result<()> {
    let s = serde_json::to_string(v).unwrap_or_default();
    let cur: Option<String> = c.query_row("SELECT v FROM kv WHERE k = ?", [k], |r| r.get(0)).optional()?;
    if cur.as_deref() != Some(s.as_str()) {
        c.execute("INSERT OR REPLACE INTO kv (k, v) VALUES (?, ?)", params![k, s])?;
    }
    Ok(())
}

pub fn kv_del(c: &Connection, k: &str) -> rusqlite::Result<()> {
    c.execute("DELETE FROM kv WHERE k = ?", [k]).map(|_| ())
}

/// Store a feed event: `{id, ts, ...e}` and send it to owner sockets. The feed keeps the newest 5000.
/// TODO(rust-port): MCP subscribers and Web Push once those are ported.
pub fn append(app: &App, c: &Connection, e: Value) -> Value {
    let now = app.now();
    let mut ev = Map::new();
    ev.insert("id".into(), Value::from(uuid::Uuid::new_v4().to_string()));
    ev.insert("ts".into(), Value::from(app.iso(now)));
    if let Value::Object(m) = e {
        for (k, v) in m {
            ev.insert(k, v);
        }
    }
    let ev = Value::Object(ev);
    let r = ev.get("ref").and_then(Value::as_str).map(str::to_string);
    let ts = ev.get("ts").and_then(Value::as_str).unwrap_or_default().to_string();
    let text = ev.to_string();
    if let Err(e) = c.execute("INSERT INTO events (ts, json, ref) VALUES (?, ?, ?)", params![ts, text, r]) {
        tracing::error!("append event: {e}");
    }
    let _ = app.owner_tx.send(crate::app::Frame::Event(std::sync::Arc::from(text)));
    let mut b = [0u8; 1];
    noop_core::random_bytes(&mut b);
    if b[0] < 5 {
        // ~2% of appends, as the Worker did
        let _ = c.execute("DELETE FROM events WHERE seq <= (SELECT MAX(seq) - 5000 FROM events)", []);
    }
    if let Err(e) = crate::feed::prune_media(app, c, false) {
        tracing::error!("prune media: {e}");
    }
    ev
}
