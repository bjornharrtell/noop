//! Shared fleet files. Port of cloud/src/files.ts; the API is unchanged:
//!
//!   GET  /api/files/changes?since=<rev>[&limit=]  → {rev, more, files:[{path, version, hash, deleted, content(base64)|null, rev}]}
//!   POST /api/files/commit {msg, changes:[{path, base_version, content(base64)} | {path, base_version, deleted:true}]}
//!        → 200 {rev, files:[{path, version, hash, deleted}]} | 409 {conflicts:[path]} (nothing applied) | 400/413 {error}
//!        Compare-and-swap per path: base_version must equal the current version (0 = must not exist / be deleted).
//!        This is the distributed lock task claims rely on (exactly one host wins a move).
//!   GET  /api/files/history?path=&limit=[&content=1] → {history:[{seq, version, hash, ts, by, msg, deleted, size, content?}]}
//!   POST /api/files/revert {path, version, msg?} → {rev, version}
//!   GET  /api/files/list?prefix=<dir/> → {files:[{path, size, updated, version}]}   (READABLE prefixes only)
//!   GET  /api/files/read?path=        → {path, content (utf8), size, updated, version} | 404
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use axum::http::Method;
use axum::response::Response;
use noop_core::budget::{latest_only, LATEST_ONLY_SQL, STORAGE_TARGET_BYTES};
use noop_core::paths::{prefix_end, readable, valid_path};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::app::{err, iso, js_num, js_num_value, json, num, ok, or, sha256hex, Req};

/// Bytes per file.
pub const MAX_FILE: usize = 1_500_000;
/// Response budget for one changes page.
const PAGE_BYTES: usize = 8_000_000;
const PAGE_ROWS: f64 = 1000.0;
const HISTORY_DAYS: i64 = 90;
const HISTORY_ROWS: i64 = 20_000;
const HISTORY_PER_PATH: i64 = 30;
const LIST_ROWS: i64 = 2000;

/// Standard alphabet, padding optional (what `atob` accepts).
const B64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent).with_decode_allow_trailing_bits(true),
);

pub fn b64enc(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

pub fn b64dec(s: &str) -> Option<Vec<u8>> {
    let clean: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    B64.decode(clean).ok()
}

struct Row {
    version: i64,
    deleted: bool,
}

fn row_of(c: &Connection, path: &str) -> rusqlite::Result<Option<Row>> {
    c.query_row("SELECT version, deleted FROM files WHERE path = ?", [path], |r| Ok(Row { version: r.get(0)?, deleted: r.get::<_, i64>(1)? != 0 }))
        .optional()
}

/// Version a writer must name as base: 0 when absent or deleted.
fn live_version(r: &Option<Row>) -> i64 {
    match r {
        Some(r) if !r.deleted => r.version,
        _ => 0,
    }
}

fn cur_rev(c: &Connection) -> rusqlite::Result<i64> {
    Ok(c.query_row("SELECT v FROM files_meta WHERE k = 'rev'", [], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0))
}

pub struct Prepared {
    pub path: String,
    pub base: i64,
    pub content: Option<Vec<u8>>,
    pub hash: Option<String>,
}

pub enum Cas {
    Applied { rev: i64, files: Vec<Value> },
    Conflicts(Vec<String>),
}

/// Check-and-apply in one transaction, under the connection lock: no other request can interleave.
pub fn apply_cas(c: &mut Connection, items: &[Prepared], actor: &str, msg: &str, now: i64) -> rusqlite::Result<Cas> {
    let tx = c.transaction()?;
    let mut conflicts = Vec::new();
    for it in items {
        if live_version(&row_of(&tx, &it.path)?) != it.base {
            conflicts.push(it.path.clone());
        }
    }
    if !conflicts.is_empty() {
        return Ok(Cas::Conflicts(conflicts));
    }
    let mut rev = cur_rev(&tx)?;
    let ts = iso(now);
    let mut out = Vec::new();
    for it in items {
        let r = row_of(&tx, &it.path)?;
        if it.content.is_none() && live_version(&r) == 0 {
            // delete of nothing
            out.push(json!({ "path": it.path, "version": r.as_ref().map_or(0, |r| r.version), "hash": null, "deleted": true }));
            continue;
        }
        let version = r.as_ref().map_or(0, |r| r.version) + 1;
        rev += 1;
        // A live path rewritten in place leaves path and deleted alone, so files_live (index rows are billed upstream) is
        // not rewritten either: most writes — heartbeats, journals — touch no index row, and only a path created or
        // deleted, which is a task move, does (1ac7e6a).
        if it.content.is_some() && live_version(&r) > 0 {
            tx.execute(
                "UPDATE files SET content = ?, hash = ?, version = ?, updated = ?, by = ?, rev = ? WHERE path = ?",
                params![it.content, it.hash, version, ts, actor, rev, it.path],
            )?;
        } else {
            tx.execute(
                "INSERT INTO files (path, content, hash, version, updated, by, deleted, rev) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(path) DO UPDATE SET content = excluded.content, hash = excluded.hash, version = excluded.version,
                   updated = excluded.updated, by = excluded.by, deleted = excluded.deleted, rev = excluded.rev",
                params![it.path, it.content, it.hash, version, ts, actor, it.content.is_none() as i64, rev],
            )?;
        }
        if !latest_only(&it.path) {
            tx.execute(
                "INSERT INTO history (path, version, hash, content, ts, by, msg) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![it.path, version, it.hash, it.content, ts, actor, msg],
            )?;
        }
        out.push(json!({ "path": it.path, "version": version, "hash": it.hash, "deleted": it.content.is_none() }));
    }
    tx.execute("INSERT OR REPLACE INTO files_meta (k, v) VALUES ('rev', ?)", [rev])?;
    let db_bytes = crate::db::size(&tx);
    prune_history(&tx, now, db_bytes)?;
    tx.commit()?;
    Ok(Cas::Applied { rev, files: out })
}

/// History keeps ≤ 90 days, ≤ 20k rows and ≤ 30 versions per path, but always the latest row of each path. Once a day.
pub fn prune_history(c: &Connection, now: i64, db_bytes: i64) -> rusqlite::Result<()> {
    let last: i64 = c.query_row("SELECT v FROM files_meta WHERE k = 'pruned'", [], |r| r.get(0)).optional()?.unwrap_or(0);
    if now - last < 86_400_000 {
        return Ok(());
    }
    c.execute("INSERT OR REPLACE INTO files_meta (k, v) VALUES ('pruned', ?)", [now])?;
    let cutoff = iso(now - HISTORY_DAYS * 86_400_000);
    c.execute(
        "DELETE FROM history WHERE (ts < ? OR seq <= (SELECT MAX(seq) FROM history) - ?)
         AND seq NOT IN (SELECT MAX(seq) FROM history GROUP BY path)",
        params![cutoff, HISTORY_ROWS],
    )?;
    c.execute(&format!("DELETE FROM history WHERE {LATEST_ONLY_SQL}"), [])?;
    c.execute(
        "DELETE FROM history WHERE seq IN (SELECT seq FROM (SELECT seq,
           ROW_NUMBER() OVER (PARTITION BY path ORDER BY seq DESC) AS rn FROM history) WHERE rn > ?)",
        [if db_bytes > STORAGE_TARGET_BYTES { 1 } else { HISTORY_PER_PATH }],
    )?;
    Ok(())
}

/// Read-only routes for the owner (and machines): a primary-key range on `files`, never the history table.
pub fn handle_read(c: &Connection, req: &Req) -> rusqlite::Result<Option<Response>> {
    if req.path == "/api/files/list" {
        let mut prefix = req.q("prefix").unwrap_or("").to_string();
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        if !readable(&prefix) {
            return Ok(Some(err("prefix not readable", 403)));
        }
        let mut st = c.prepare_cached(
            "SELECT path, LENGTH(content), updated, version FROM files
             WHERE path >= ? AND path < ? AND deleted = 0 ORDER BY path LIMIT ?",
        )?;
        let files = st
            .query_map(params![prefix, prefix_end(&prefix), LIST_ROWS], |r| {
                Ok(json!({ "path": r.get::<_, String>(0)?, "size": r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    "updated": r.get::<_, Option<String>>(2)?, "version": r.get::<_, i64>(3)? }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        return Ok(Some(ok(json!({ "files": files }))));
    }
    if req.path == "/api/files/read" {
        let path = req.q("path").unwrap_or("");
        if !valid_path(path) || !readable(path) {
            return Ok(Some(err("path not readable", 403)));
        }
        let r = c
            .query_row("SELECT content, updated, version FROM files WHERE path = ? AND deleted = 0", [path], |r| {
                Ok((r.get::<_, Option<Vec<u8>>>(0)?.unwrap_or_default(), r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?))
            })
            .optional()?;
        let Some((content, updated, version)) = r else { return Ok(Some(err("not found", 404))) };
        return Ok(Some(ok(json!({ "path": path, "content": String::from_utf8_lossy(&content), "size": content.len(),
            "updated": updated, "version": version }))));
    }
    Ok(None)
}

/// The fleet routes. `None` when the path is not under /api/files/.
pub fn handle(c: &mut Connection, req: &Req, actor: &str, now: i64) -> rusqlite::Result<Option<Response>> {
    let p = req.path.as_str();
    if !p.starts_with("/api/files/") {
        return Ok(None);
    }
    let get = req.method == Method::GET;
    let post = req.method == Method::POST;

    if p == "/api/files/changes" && get {
        let since = or(js_num(req.q("since")), 0.0).max(0.0);
        let limit = or(js_num(Some(req.q("limit").unwrap_or("1000"))), PAGE_ROWS).clamp(1.0, PAGE_ROWS);
        let head = cur_rev(c)?;
        let mut files = Vec::new();
        let (mut size, mut last, mut more) = (0usize, since, false);
        let mut st = c.prepare_cached("SELECT path, content, hash, version, deleted, rev FROM files WHERE rev > ? ORDER BY rev LIMIT 1")?;
        // one row at a time, so a page never loads more than its byte budget
        loop {
            let row = st
                .query_row([last], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<Vec<u8>>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)? != 0,
                        r.get::<_, i64>(5)?,
                    ))
                })
                .optional()?;
            let Some((path, content, hash, version, deleted, rev)) = row else { break };
            let content = if deleted { None } else { Some(content.unwrap_or_default()) };
            let len = content.as_ref().map_or(0, Vec::len);
            if !files.is_empty() && (files.len() as f64 >= limit || size + len > PAGE_BYTES) {
                more = true;
                break;
            }
            files.push(json!({ "path": path, "version": version, "hash": hash, "deleted": deleted,
                "content": content.as_deref().map(b64enc), "rev": rev }));
            size += len;
            last = rev as f64;
        }
        let rev = if more { last } else { last.max(head as f64) };
        return Ok(Some(ok(json!({ "rev": num(rev), "more": more, "files": files }))));
    }

    if p == "/api/files/commit" && post {
        let b = req.json();
        let Some(changes) = b.as_ref().and_then(|b| b.get("changes")).and_then(Value::as_array).filter(|a| !a.is_empty()) else {
            return Ok(Some(err("changes[] required", 400)));
        };
        let mut seen = std::collections::HashSet::new();
        let mut items = Vec::new();
        for ch in changes {
            let path = ch.get("path").and_then(Value::as_str).filter(|p| valid_path(p));
            let Some(path) = path else {
                return Ok(Some(err(format!("invalid path: {}", crate::app::js_string(ch.get("path"), 200)), 400)));
            };
            if !seen.insert(path.to_string()) {
                return Ok(Some(err(format!("duplicate path: {path}"), 400)));
            }
            let base = js_num_value(ch.get("base_version"));
            if base.is_nan() || base.fract() != 0.0 || base < 0.0 {
                return Ok(Some(err(format!("bad base_version for {path}"), 400)));
            }
            let base = base as i64;
            if ch.get("deleted") == Some(&Value::Bool(true)) {
                items.push(Prepared { path: path.into(), base, content: None, hash: None });
                continue;
            }
            let Some(s) = ch.get("content").and_then(Value::as_str) else {
                return Ok(Some(err(format!("content (base64) or deleted:true required for {path}"), 400)));
            };
            let Some(content) = b64dec(s) else { return Ok(Some(err(format!("content is not base64: {path}"), 400))) };
            if content.len() > MAX_FILE {
                return Ok(Some(err(
                    format!("file too large: {path} is {} bytes (max {MAX_FILE}); keep large artifacts outside the data folder", content.len()),
                    413,
                )));
            }
            let hash = sha256hex(&content);
            items.push(Prepared { path: path.into(), base, content: Some(content), hash: Some(hash) });
        }
        let msg = match b.as_ref().and_then(|b| b.get("msg")) {
            None | Some(Value::Null) => String::new(),
            v => crate::app::js_string(v, 500),
        };
        return Ok(Some(match apply_cas(c, &items, actor, &msg, now)? {
            Cas::Conflicts(conflicts) => json(json!({ "conflicts": conflicts }), 409),
            Cas::Applied { rev, files } => ok(json!({ "rev": rev, "files": files })),
        }));
    }

    if p == "/api/files/history" && get {
        let path = req.q("path").unwrap_or("");
        if !valid_path(path) {
            return Ok(Some(err("path required", 400)));
        }
        let limit = or(js_num(Some(req.q("limit").unwrap_or("50"))), 50.0).clamp(1.0, 500.0) as i64;
        let with_content = req.q("content") == Some("1");
        type H = (i64, i64, Option<String>, Option<Vec<u8>>, Option<String>, Option<String>, Option<String>);
        let map = |r: &rusqlite::Row| -> rusqlite::Result<H> {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        };
        let rows: Vec<H> = if latest_only(path) {
            // no history kept: the current version is all there is
            c.prepare_cached("SELECT 0, version, hash, CASE WHEN deleted THEN NULL ELSE content END, updated, by, '' FROM files WHERE path = ?")?
                .query_map([path], map)?
                .collect::<rusqlite::Result<_>>()?
        } else {
            c.prepare_cached("SELECT seq, version, hash, content, ts, by, msg FROM history WHERE path = ? ORDER BY seq DESC LIMIT ?")?
                .query_map(params![path, limit], map)?
                .collect::<rusqlite::Result<_>>()?
        };
        let history: Vec<Value> = rows
            .into_iter()
            .map(|(seq, version, hash, content, ts, by, msg)| {
                let mut h = json!({ "seq": seq, "version": version, "hash": hash, "ts": ts, "by": by, "msg": msg,
                    "deleted": content.is_none(), "size": content.as_ref().map_or(0, Vec::len) });
                if with_content {
                    h["content"] = content.as_deref().map(b64enc).into();
                }
                h
            })
            .collect();
        return Ok(Some(ok(json!({ "history": history }))));
    }

    if p == "/api/files/revert" && post {
        let b = req.json().unwrap_or(Value::Null);
        let path = b.get("path").and_then(Value::as_str).filter(|p| valid_path(p));
        let version = b.get("version").and_then(Value::as_f64).filter(|v| v.fract() == 0.0);
        let (Some(path), Some(version)) = (path, version) else { return Ok(Some(err("{path, version} required", 400))) };
        let version = version as i64;
        let h = c
            .query_row("SELECT content, hash FROM history WHERE path = ? AND version = ? ORDER BY seq DESC LIMIT 1", params![path, version], |r| {
                Ok((r.get::<_, Option<Vec<u8>>>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .optional()?;
        let Some((content, hash)) = h else {
            return Ok(Some(err(format!("no history for {path} v{version} (pruned or never existed)"), 404)));
        };
        let base = live_version(&row_of(c, path)?);
        let msg = match b.get("msg") {
            None | Some(Value::Null) => format!("revert {path} to v{version}"),
            v => crate::app::js_string(v, 500),
        };
        let hash = if content.is_some() { Some(hash.unwrap_or_else(|| "null".into())) } else { None };
        let item = Prepared { path: path.into(), base, content, hash };
        return Ok(Some(match apply_cas(c, &[item], actor, &crate::app::truncate(&msg, 500), now)? {
            Cas::Conflicts(conflicts) => json(json!({ "conflicts": conflicts }), 409),
            Cas::Applied { rev, files } => ok(json!({ "rev": rev, "version": files[0]["version"] })),
        }));
    }

    Ok(Some(err("not found", 404)))
}
