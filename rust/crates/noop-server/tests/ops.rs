//! Moving data between servers, the IP allow-list, usage.
mod common;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use common::*;
use noop_server::app::{App, Config};
use serde_json::json;

fn cfg() -> Config {
    Config { hub_token: TOKEN.into(), owners: vec![OWNER.into()], en: true, ..Default::default() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_everything_to_a_new_server() {
    // the old server, with files, history, a signed-in owner, a secret, media and feed events
    let old = T::new();
    let (session, _) = old.sign_in().await;
    old.machine("POST", "/api/files/commit", Some(json!({ "changes": [{ "path": "memory/a.md", "base_version": 0, "content": b64(b"v1") }] }))).await;
    old.machine("POST", "/api/files/commit", Some(json!({ "changes": [{ "path": "memory/a.md", "base_version": 1, "content": b64(b"v2") }] }))).await;
    old.call("PUT", "/api/secrets", Some(&session), Some(json!({ "name": "API_KEY", "value": "s3cret" }))).await;
    let blob: Vec<u8> = (0..2_200_000u32).map(|i| (i % 253) as u8).collect();
    let (_, m) = old.upload("/api/media", TOKEN, Some("big.bin"), Some("application/octet-stream"), &blob).await;
    for i in 0..7 {
        old.machine("POST", "/api/events", Some(json!({ "from": "dev", "kind": "result", "text": format!("event {i}") }))).await;
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(noop_server::serve_on(old.app.clone(), l));

    // the new server's database, filled from the old one
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    noop_server::db::init(&conn).unwrap();
    let log = std::sync::Mutex::new(Vec::new());
    noop_server::ops::import_from(&mut conn, &base, TOKEN, |l| log.lock().unwrap().push(l)).await.unwrap();
    let log = log.into_inner().unwrap();
    assert!(log.iter().any(|l| l.starts_with("  media: 3 of 3")), "{log:?}");
    assert!(log.iter().any(|l| l.starts_with("  files: 1 of 1")), "{log:?}");

    let clock = Arc::new(AtomicI64::new(old.now()));
    let c2 = clock.clone();
    let new = T { app: App::new(conn, cfg(), Some(Arc::new(move || c2.load(Ordering::SeqCst)))).unwrap(), clock };
    // the owner stays signed in, the secret opens, files and their history, media and the feed came over
    let (s, r) = new.call("GET", "/api/files/history?path=memory/a.md", Some(&session), None).await;
    assert_eq!((s, r["history"].as_array().map(Vec::len)), (200, Some(2)));
    let (_, r) = new.machine("GET", "/api/secrets/values", None).await;
    assert_eq!(r["values"]["API_KEY"], "s3cret");
    let (s, body, _) = new.bytes("GET", &format!("{}?token={TOKEN}", m["url"].as_str().unwrap()), &[]).await;
    assert_eq!((s, body == blob), (200, true));
    let (_, f) = new.call("GET", "/api/feed?since=", Some(&session), None).await;
    assert!(f["events"].as_array().unwrap().iter().any(|e| e["text"] == "event 6"));
    // new writes continue the sequence instead of reusing ids
    let (_, r) = new.machine("POST", "/api/files/commit", Some(json!({ "changes": [{ "path": "memory/a.md", "base_version": 2, "content": b64(b"v3") }] }))).await;
    assert_eq!(r["files"][0]["version"], 3);
}

#[tokio::test]
async fn export_and_import_routes() {
    let t = T::new();
    let (session, _) = t.sign_in().await;
    let (_, r) = t.machine("GET", "/api/host/export", None).await;
    assert!(r["tables"]["sessions"].as_i64() == Some(1), "{r}");
    let (s, _) = t.call("GET", "/api/host/export", Some(&session), None).await;
    assert_eq!(s, 403, "machine only");
    let (s, _) = t.machine("GET", "/api/host/export?table=nope", None).await;
    assert_eq!(s, 404);
    let (s, r) = t.machine("POST", "/api/host/import", Some(json!({ "table": "kv", "rows": [{ "k": "x", "v": "\"y\"", "_rowid": 9 }] }))).await;
    assert_eq!((s, r["n"].as_i64()), (200, Some(1)));
    let (s, r) = t.machine("POST", "/api/host/import", Some(json!({ "table": "kv", "rows": [{ "k\" ) ; DROP TABLE kv; --": 1 }] }))).await;
    assert_eq!(s, 400, "unknown columns are refused, not spliced into SQL: {r}");
    let (_, r) = t.machine("GET", "/api/host/export?table=kv&after=0&limit=1000", None).await;
    assert!(r["rows"].as_array().unwrap().iter().any(|row| row["k"] == "x"));
}

#[tokio::test]
async fn allow_list_gates_every_path() {
    let mut c = cfg();
    c.allow_ips = Some(noop_core::ipallow::parse_allow_list("203.0.113.0/24").0);
    c.client_ip_header = Some("x-forwarded-for".into());
    let t = T::with(c);
    let (s, body, _) = t.raw("GET", "/api/status", &[("x-forwarded-for", "198.51.100.7, 10.0.0.1")]).await;
    assert_eq!(s, 403);
    assert!(body.contains("\"ip\":\"198.51.100.7\""), "{body}");
    let (s, body, h) = t.raw("GET", "/", &[("x-forwarded-for", "198.51.100.7"), ("accept", "text/html")]).await;
    assert_eq!((s, h["content-type"].as_str()), (403, "text/html; charset=utf-8"));
    assert!(body.contains("198.51.100.7"));
    let (s, _, _) = t.raw("GET", "/api/status", &[("x-forwarded-for", "203.0.113.9")]).await;
    assert_eq!(s, 401, "allowed through to normal auth");
    let (s, _, _) = t.raw("GET", "/api/status", &[]).await;
    assert_eq!(s, 403, "no client address: refused");
}

#[test]
fn backup_succeeds_twice_in_one_day() {
    // The nightly timer writes one file per day; a manual run or a retry on the same day used to fail because
    // VACUUM INTO refuses an existing file.
    let dir = std::env::temp_dir().join(format!("noop-backup-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("noop.db");
    let out = dir.join("backup.db");
    let (db, out) = (db.to_str().unwrap(), out.to_str().unwrap());

    let c = rusqlite::Connection::open(db).unwrap();
    noop_server::db::init(&c).unwrap();
    c.execute("INSERT INTO kv (k, v) VALUES ('k', 'v')", []).unwrap();
    drop(c);

    noop_server::ops::backup(db, out).unwrap();
    let first = std::fs::metadata(out).unwrap().len();
    // the second run overwrites, and still produces a complete, readable copy
    noop_server::ops::backup(db, out).unwrap();

    let copy = rusqlite::Connection::open(out).unwrap();
    let v: String = copy.query_row("SELECT v FROM kv WHERE k = 'k'", [], |r| r.get(0)).unwrap();
    let ok: String = copy.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
    assert_eq!(v, "v");
    assert_eq!(ok, "ok");
    assert!(first > 0);
    assert!(!std::path::Path::new(&format!("{out}.tmp")).exists(), "the temporary copy is gone");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn usage_counts_requests() {
    let t = T::new();
    t.machine("GET", "/api/status", None).await;
    t.machine("GET", "/api/tasks/get?id=T-0001", None).await;
    let (_, u) = t.machine("GET", "/api/usage", None).await;
    assert_eq!(u["routes"]["GET /api/tasks/get"], 1);
    assert!(u["requests"].as_i64().unwrap() >= 3);
    assert!(u["dbBytes"].as_i64().unwrap() > 0);
    assert!(u.get("free").is_none(), "no Cloudflare plan: `noop status` prints no plan percentages");
}
