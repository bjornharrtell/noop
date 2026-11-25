//! Operating the server: the IP allow-list gate, moving data in and out, and today's usage.
//!
//!   GET  /api/host/export                         (machine) → {tables: {name: rows}}
//!   GET  /api/host/export?table=&after=&limit=    (machine) → {rows}   walks by rowid; blobs as {"$b64": …}
//!   POST /api/host/import {table, rows, reset}    (machine)            reset empties the table first
//!   GET  /api/usage                               → {day, requests, routes, level, dbBytes, sockets}
//!
//! Export and import have the Worker's shapes, so a Cloudflare deployment's data comes over with
//! `noop-server import-from <worker url>` (see main.rs), page by page.
use std::collections::BTreeMap;
use std::sync::Mutex;

use axum::body::Body;
use axum::http::Method;
use axum::response::Response;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rusqlite::types::{Value as Sql, ValueRef};
use rusqlite::Connection;
use serde_json::{json, Map, Value};

use crate::app::{err, ok, App, Req};
use crate::auth::Who;

// ---------------------------------------------------------------- allow-list

/// 403 for a source IP not on HUB_ALLOW_IPS. The visitor sees its own public IP (nothing else), so the owner can add a
/// phone's network from the phone itself: JSON for the app, a small page for a browser.
pub fn ip_gate(app: &App, req: &Req) -> Option<Response> {
    let nets = app.cfg.allow_ips.as_ref()?;
    let ip = req.ip(&app.cfg);
    if noop_core::ipallow::ip_allowed(&ip, nets) {
        return None;
    }
    let shown = if ip.is_empty() { app.l("unknown", "未知") } else { ip.clone() };
    let msg = app.l(
        &format!("This network's public IP {shown} is not on the allow-list (HUB_ALLOW_IPS)"),
        &format!("这个网络的出口 IP {shown} 不在允许名单（HUB_ALLOW_IPS）里"),
    );
    let b = Response::builder().status(403).header("cache-control", "no-store").header("x-content-type-options", "nosniff");
    if !req.header("accept").unwrap_or("").contains("text/html") {
        return Some(b.header("content-type", "application/json").body(Body::from(json!({ "error": "ip_blocked", "ip": ip, "message": msg }).to_string())).expect("valid"));
    }
    let esc = |t: &str| t.chars().map(|c| if "&<>\"'".contains(c) { format!("&#{};", c as u32) } else { c.to_string() }).collect::<String>();
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>:root{{color-scheme:light dark}}body{{margin:0;font:16px/1.6 -apple-system,system-ui,sans-serif;background:Canvas;color:CanvasText}}main{{max-width:420px;margin:18vh auto;padding:0 16px}}h1{{font-size:20px;margin:0 0 12px}}.ip{{font:600 24px/1.3 ui-monospace,Menlo,monospace;user-select:all;padding:12px 14px;border:1px solid #8884;border-radius:8px;word-break:break-all}}p{{color:GrayText;font-size:14px}}</style></head><body><main><h1>{}</h1><div class=\"ip\">{}</div><p>{}</p></main></body></html>",
        app.l("Noop: IP not allowed", "Noop：IP 未放行"),
        app.l("This network is not allowed yet", "这个网络还没放行"),
        esc(&shown),
        app.l("This is your current network's public IP. To use Noop from here, add it to the server's HUB_ALLOW_IPS (ranges work, e.g. 1.2.3.0/24; mobile data often uses changing IPv6, so add its /64).", "这是你当前网络的出口 IP。要从这里使用 Noop，把它加到服务器的 HUB_ALLOW_IPS 里（可以写网段，如 1.2.3.0/24；手机流量常用 IPv6，地址会变，写它的 /64 网段）。"),
    );
    Some(b.header("content-type", "text/html; charset=utf-8").header("content-security-policy", "default-src 'none'; style-src 'unsafe-inline'").body(Body::from(html)).expect("valid"))
}

// ---------------------------------------------------------------- export / import

fn tables(c: &Connection) -> rusqlite::Result<Vec<String>> {
    c.prepare_cached("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '_cf_%' ORDER BY name")?
        .query_map([], |r| r.get(0))?
        .collect()
}

fn columns(c: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    c.prepare(&format!("PRAGMA table_info(\"{table}\")"))?.query_map([], |r| r.get(1))?.collect()
}

fn to_json(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => i.into(),
        ValueRef::Real(f) => f.into(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
        ValueRef::Blob(b) => json!({ "$b64": STANDARD.encode(b) }),
    }
}

fn to_sql(v: &Value) -> Sql {
    match v {
        Value::Null => Sql::Null,
        Value::Bool(b) => Sql::Integer(*b as i64),
        Value::Number(n) => n.as_i64().map(Sql::Integer).unwrap_or_else(|| Sql::Real(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => Sql::Text(s.clone()),
        Value::Object(o) if o.get("$b64").is_some_and(Value::is_string) => Sql::Blob(STANDARD.decode(o["$b64"].as_str().unwrap_or("")).unwrap_or_default()),
        other => Sql::Text(other.to_string()),
    }
}

/// One page of a table: rows after `after` (rowid), each with `_rowid` for the next page.
pub fn export_page(c: &Connection, table: &str, after: i64, limit: i64) -> rusqlite::Result<Vec<Value>> {
    let mut st = c.prepare(&format!("SELECT rowid AS _rowid, * FROM \"{table}\" WHERE rowid > ? ORDER BY rowid LIMIT ?"))?;
    let names: Vec<String> = st.column_names().map(String::from).collect();
    let rows = st.query_map(rusqlite::params![after, limit], |r| {
        let mut o = Map::new();
        for (i, n) in names.iter().enumerate() {
            o.insert(n.clone(), to_json(r.get_ref(i)?));
        }
        Ok(Value::Object(o))
    })?;
    rows.collect()
}

/// Write rows into a table (columns checked against the table), in one transaction. Returns rows written.
pub fn import_rows(c: &mut Connection, table: &str, rows: &[Value], reset: bool) -> Result<usize, String> {
    let known = tables(c).map_err(|e| e.to_string())?;
    if !known.iter().any(|t| t == table) {
        return Err("no such table".into());
    }
    let cols = columns(c, table).map_err(|e| e.to_string())?;
    let tx = c.transaction().map_err(|e| e.to_string())?;
    if reset {
        tx.execute(&format!("DELETE FROM \"{table}\""), []).map_err(|e| e.to_string())?;
    }
    for r in rows {
        let Some(o) = r.as_object() else { return Err("rows must be objects".into()) };
        let keys: Vec<&String> = o.keys().filter(|k| k.as_str() != "_rowid").collect(); // the export's paging key
        if let Some(k) = keys.iter().find(|k| !cols.contains(k)) {
            return Err(format!("no column {k} in {table}"));
        }
        if keys.is_empty() {
            continue;
        }
        let sql = format!(
            "INSERT OR REPLACE INTO \"{table}\" ({}) VALUES ({})",
            keys.iter().map(|k| format!("\"{k}\"")).collect::<Vec<_>>().join(","),
            keys.iter().map(|_| "?").collect::<Vec<_>>().join(",")
        );
        let vals: Vec<Sql> = keys.iter().map(|k| to_sql(&o[k.as_str()])).collect();
        tx.execute(&sql, rusqlite::params_from_iter(vals)).map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(rows.len())
}

/// Copy every table a Worker (or another noop-server) exports into `conn`, page by page; each table is replaced as a
/// whole. Stop the server that uses `conn` first.
pub async fn import_from(conn: &mut Connection, url: &str, token: &str, log: impl Fn(String)) -> Result<(), String> {
    let http = reqwest::Client::new();
    let get = |path: String| {
        let req = http.get(format!("{url}{path}")).bearer_auth(token).timeout(std::time::Duration::from_secs(120));
        async move {
            let r = req.send().await.map_err(|e| e.to_string())?;
            let status = r.status();
            let body: Value = r.json().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(format!("HTTP {status}: {body}"));
            }
            Ok::<Value, String>(body)
        }
    };
    let counts = get("/api/host/export".into()).await?;
    let Some(tables) = counts.get("tables").and_then(Value::as_object) else { return Err(format!("unexpected answer: {counts}")) };
    for (table, total) in tables {
        // media rows are 1 MB chunks: small pages
        let limit = if table == "media" { 5 } else { 500 };
        let (mut after, mut copied, mut first) = (0i64, 0usize, true);
        loop {
            let page = get(format!("/api/host/export?table={table}&after={after}&limit={limit}")).await?;
            let rows = page.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
            if rows.is_empty() && !first {
                break;
            }
            match import_rows(conn, table, &rows, first) {
                Ok(n) => copied += n,
                Err(e) if e == "no such table" => {
                    log(format!("  {table}: skipped (not used by this server)"));
                    break;
                }
                Err(e) => return Err(format!("{table}: {e}")),
            }
            first = false;
            match rows.last().and_then(|r| r.get("_rowid")).and_then(Value::as_i64) {
                Some(last) if rows.len() == limit => after = last,
                _ => break,
            }
        }
        log(format!("  {table}: {copied} of {total}"));
    }
    log("done".into());
    Ok(())
}

// ---------------------------------------------------------------- usage

/// Requests per route today (UTC), in memory: a restart starts the day's count over.
#[derive(Default)]
pub struct Usage {
    inner: Mutex<(String, u64, BTreeMap<String, u64>)>,
}

impl Usage {
    pub fn count(&self, day: &str, route: String) {
        let mut u = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if u.0 != day {
            *u = (day.to_string(), 0, BTreeMap::new());
        }
        u.1 += 1;
        *u.2.entry(route).or_default() += 1;
    }
    fn view(&self) -> (String, u64, BTreeMap<String, u64>) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// "POST /api/files/commit": the route without ids, so a day's breakdown stays a few dozen keys.
pub fn route_key(method: &Method, path: &str) -> String {
    let p = if path.starts_with("/media/") {
        "/media/:id".to_string()
    } else {
        path.split('/').map(|s| if s.starts_with("T-") || s.starts_with("D-") { ":id" } else { s }).collect::<Vec<_>>().join("/")
    };
    format!("{method} {}", p.chars().take(40).collect::<String>())
}

// ---------------------------------------------------------------- routes

pub fn routes(app: &App, req: &Req, w: &Who) -> rusqlite::Result<Option<Response>> {
    let machine = *w == Who::Machine;
    let p = req.path.as_str();
    if p == "/api/usage" && req.method == Method::GET {
        let (day, requests, routes) = app.usage.view();
        let db_bytes = crate::db::size(&app.db());
        return Ok(Some(ok(json!({ "day": day, "requests": requests, "routes": routes, "level": "ok", "dbBytes": db_bytes,
            "sockets": { "owner": app.owner_sockets(), "host": app.host_sockets() } }))));
    }
    if p == "/api/host/export" && req.method == Method::GET {
        if !machine {
            return Ok(Some(err("forbidden", 403)));
        }
        let c = app.db();
        let names = tables(&c)?;
        let Some(t) = req.q("table") else {
            let mut counts = Map::new();
            for n in &names {
                let k: i64 = c.query_row(&format!("SELECT COUNT(*) FROM \"{n}\""), [], |r| r.get(0))?;
                counts.insert(n.clone(), k.into());
            }
            return Ok(Some(ok(json!({ "tables": counts }))));
        };
        if !names.iter().any(|n| n == t) {
            return Ok(Some(err("no such table", 404)));
        }
        let limit = crate::app::js_num(Some(req.q("limit").unwrap_or("100")));
        let limit = if limit.is_nan() { 100 } else { (limit as i64).clamp(1, 1000) };
        // ?after=<rowid of the last row> walks by key; ?offset= (old copiers) is mapped onto the same walk
        let after = match (req.q("after"), req.q("offset")) {
            (Some(a), _) => a.parse::<i64>().unwrap_or(0),
            (None, Some(o)) => {
                let o = o.parse::<i64>().unwrap_or(0).max(0);
                if o == 0 { 0 } else {
                    c.query_row(&format!("SELECT rowid FROM \"{t}\" ORDER BY rowid LIMIT 1 OFFSET ?"), [o - 1], |r| r.get(0)).unwrap_or(i64::MAX)
                }
            }
            _ => 0,
        };
        return Ok(Some(ok(json!({ "rows": export_page(&c, t, after, limit)? }))));
    }
    if p == "/api/host/import" && req.method == Method::POST {
        if !machine {
            return Ok(Some(err("forbidden", 403)));
        }
        let Some(b) = req.json() else { return Ok(Some(err("bad json", 400))) };
        let table = b.get("table").and_then(Value::as_str).unwrap_or("");
        let rows = b.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
        let reset = b.get("reset") == Some(&Value::Bool(true));
        let mut c = app.db();
        return Ok(Some(match import_rows(&mut c, table, &rows, reset) {
            Ok(n) => ok(json!({ "ok": true, "n": n })),
            Err(e) if e == "no such table" => err(e, 404),
            Err(e) => err(e, 400),
        }));
    }
    Ok(None)
}
