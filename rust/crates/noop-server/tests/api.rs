//! API contract tests. The files part ports cloud/test/run.ts (same checks, same expected responses); the claim race
//! uses concurrent commits instead of the TypeScript host code. The auth part covers sign-in with code + TOTP.
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use base64::Engine;
use http_body_util::BodyExt;
use noop_server::app::{App, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

const TOKEN: &str = "machine-token";
const OWNER: &str = "owner@example.com";

struct T {
    app: Arc<App>,
    clock: Arc<AtomicI64>,
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}
fn unb64(s: &Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s.as_str().unwrap()).unwrap()
}

impl T {
    fn new() -> T {
        let clock = Arc::new(AtomicI64::new(1_780_000_000_000));
        let c2 = clock.clone();
        let cfg = Config { hub_token: TOKEN.into(), owners: vec![OWNER.into()], en: true, ..Default::default() };
        let app = App::new(rusqlite::Connection::open_in_memory().unwrap(), cfg, Some(Arc::new(move || c2.load(Ordering::SeqCst)))).unwrap();
        T { app, clock }
    }
    async fn call(&self, method: &str, path: &str, auth: Option<&str>, body: Option<Value>) -> (u16, Value) {
        let mut b = Request::builder().method(method).uri(path).header("x-noop-host", "run");
        if let Some(a) = auth {
            b = b.header("authorization", format!("Bearer {a}"));
        }
        let req = b.body(body.map(|v| Body::from(v.to_string())).unwrap_or_else(Body::empty)).unwrap();
        let res = noop_server::router(self.app.clone()).oneshot(req).await.unwrap();
        let status = res.status().as_u16();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
    async fn machine(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        self.call(method, path, Some(TOKEN), body).await
    }
    fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }
    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }
    /// Bind 2FA from a machine, sign in, and return (owner session, TOTP secret).
    async fn sign_in(&self) -> (String, String) {
        let (s, r) = self.machine("POST", "/api/host/2fa/setup", None).await;
        assert_eq!(s, 200);
        let secret = r["secret"].as_str().unwrap().to_string();
        assert!(r["uri"].as_str().unwrap().starts_with("otpauth://totp/Noop%3Aowner%40example.com?secret="));
        let (s, r) = self.machine("POST", "/api/host/2fa/confirm", Some(json!({ "totp": self.totp(&secret) }))).await;
        assert_eq!((s, &r["replaced"]), (200, &json!(false)), "{r}");
        self.advance(30_000); // the confirm used this step's code
        let (s, r) = self.call("POST", "/api/auth/start", None, Some(json!({ "email": OWNER }))).await;
        assert_eq!((s, &r["delivery"]), (200, &json!("cli")), "no SMTP: the code goes to `noop login`: {r}");
        let (s, r) = self.machine("GET", "/api/host/login-code", None).await;
        assert_eq!(s, 200);
        let code = r["code"].as_str().unwrap().to_string();
        let (s, r) = self
            .call("POST", "/api/auth/verify", None, Some(json!({ "email": OWNER, "code": code.to_lowercase(), "totp": self.totp(&secret) })))
            .await;
        assert_eq!(s, 200, "{r}");
        (r["session"].as_str().unwrap().to_string(), secret)
    }
    fn totp(&self, secret: &str) -> String {
        let key = noop_core::totp::b32decode(secret).unwrap();
        noop_core::totp::hotp(&key, noop_core::totp::step_at(self.now()) as u64)
    }
}

#[tokio::test]
async fn files_contract() {
    let t = T::new();
    let run = "r1";
    let (s, r) = t
        .machine("POST", "/api/files/commit", Some(json!({ "msg": "seed", "changes": [
            { "path": format!("{run}/a.md"), "base_version": 0, "content": b64(b"A1") },
            { "path": format!("{run}/bin"), "base_version": 0, "content": b64(&[0, 255, 1, 254]) } ] })))
        .await;
    assert!(s == 200 && r["files"].as_array().unwrap().len() == 2, "commit new files: {r}");
    let rev0 = r["rev"].as_i64().unwrap();

    let (s, r) = t
        .machine("POST", "/api/files/commit", Some(json!({ "msg": "stale", "changes": [
            { "path": format!("{run}/new.md"), "base_version": 0, "content": b64(b"x") },
            { "path": format!("{run}/a.md"), "base_version": 0, "content": b64(b"A?") } ] })))
        .await;
    assert!(s == 409 && r["conflicts"][0] == format!("{run}/a.md"), "409 on stale base: {r}");

    let (session, _) = t.sign_in().await;
    let owner = |m: &'static str, p: String, b: Option<Value>| {
        let t = &t;
        let session = session.clone();
        async move { t.call(m, &p, Some(&session), b).await }
    };
    let (_, r) = owner("GET", format!("/api/files/history?path={run}/new.md"), None).await;
    assert_eq!(r["history"].as_array().unwrap().len(), 0, "409 applied nothing");

    let (s, r) = t
        .machine("POST", "/api/files/commit", Some(json!({ "msg": "edit", "changes": [
            { "path": format!("{run}/a.md"), "base_version": 1, "content": b64(b"A2") },
            { "path": format!("{run}/bin"), "base_version": 1, "deleted": true } ] })))
        .await;
    assert!(s == 200 && r["files"][1]["deleted"] == json!(true), "CAS edit + delete: {r}");

    let (_, r) = t.machine("GET", &format!("/api/files/changes?since={}", rev0 - 2), None).await;
    let files = r["files"].as_array().unwrap();
    let a = files.iter().find(|f| f["path"] == format!("{run}/a.md")).unwrap();
    let bin = files.iter().find(|f| f["path"] == format!("{run}/bin")).unwrap();
    assert!(unb64(&a["content"]) == b"A2" && bin["deleted"] == json!(true), "changes since: {r}");

    let (_, r) = owner("GET", format!("/api/files/history?path={run}/a.md&content=1"), None).await;
    let versions: Vec<i64> = r["history"].as_array().unwrap().iter().map(|h| h["version"].as_i64().unwrap()).collect();
    assert!(versions == [2, 1] && r["history"][0]["by"] == "run", "history: {r}");

    let (s, r) = owner("POST", "/api/files/revert".into(), Some(json!({ "path": format!("{run}/a.md"), "version": 1 }))).await;
    assert!(s == 200 && r["version"] == 3, "revert: {r}");
    let (s, _) = owner("POST", "/api/files/revert".into(), Some(json!({ "path": format!("{run}/bin"), "version": 1 }))).await;
    assert_eq!(s, 200, "revert a delete restores binary");
    let (_, r) = t.machine("GET", &format!("/api/files/changes?since={rev0}"), None).await;
    let bin = r["files"].as_array().unwrap().iter().find(|f| f["path"] == format!("{run}/bin")).unwrap();
    assert_eq!(unb64(&bin["content"]), [0, 255, 1, 254], "binary round trip");

    let (mut pages, mut since, mut seen, mut more) = (0, 0i64, 0, true);
    while more {
        let (_, p) = t.machine("GET", &format!("/api/files/changes?since={since}&limit=1"), None).await;
        since = p["rev"].as_i64().unwrap();
        more = p["more"].as_bool().unwrap();
        seen += p["files"].as_array().unwrap().len();
        pages += 1;
    }
    assert!(pages > 1 && seen >= 2, "pagination: {pages} pages, {seen} rows");

    let (s, r) = t
        .machine("POST", "/api/files/commit", Some(json!({ "msg": "big", "changes": [
            { "path": format!("{run}/big"), "base_version": 0, "content": b64(&vec![0u8; 1_600_000]) } ] })))
        .await;
    assert_eq!(s, 413, "413 over 1.5 MB: {r}");
    let (s, _) = t
        .machine("POST", "/api/files/commit", Some(json!({ "msg": "bad", "changes": [{ "path": "../etc/passwd", "base_version": 0, "content": b64(b"x") }] })))
        .await;
    assert_eq!(s, 400, "400 on traversal");

    // who may call what
    let (s, _) = t.machine("GET", &format!("/api/files/history?path={run}/a.md"), None).await;
    assert_eq!(s, 403, "history is owner-only");
    let (s, _) = owner("GET", "/api/files/changes?since=0".into(), None).await;
    assert_eq!(s, 403, "sync is machine-only");
    let (s, _) = t.call("GET", "/api/files/changes?since=0", None, None).await;
    assert_eq!(s, 401);

    // owner read views: READABLE prefixes only
    t.machine("POST", "/api/files/commit", Some(json!({ "changes": [
        { "path": "memory/notes.md", "base_version": 0, "content": b64("héllo".as_bytes()) },
        { "path": "config.toml", "base_version": 0, "content": b64(b"secret") } ] })))
        .await;
    let (_, r) = owner("GET", "/api/files/list?prefix=memory".into(), None).await;
    assert_eq!(r["files"][0]["path"], "memory/notes.md");
    let (_, r) = owner("GET", "/api/files/read?path=memory/notes.md".into(), None).await;
    assert_eq!((r["content"].as_str(), r["size"].as_i64()), (Some("héllo"), Some(6)));
    let (s, _) = owner("GET", "/api/files/read?path=config.toml".into(), None).await;
    assert_eq!(s, 403);

    // journals keep no history: the current row is the history view
    t.machine("POST", "/api/files/commit", Some(json!({ "changes": [{ "path": "journal/h1.md", "base_version": 0, "content": b64(b"j1") }] })))
        .await;
    t.machine("POST", "/api/files/commit", Some(json!({ "changes": [{ "path": "journal/h1.md", "base_version": 1, "content": b64(b"j2") }] })))
        .await;
    let (_, r) = owner("GET", "/api/files/history?path=journal/h1.md".into(), None).await;
    assert_eq!(r["history"].as_array().unwrap().len(), 1);
    assert_eq!(r["history"][0]["version"], 2);
}

/// Several hosts move the same task at once: exactly one commit wins, the rest get 409 and nothing of theirs applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claim_race_has_one_winner() {
    let t = Arc::new(T::new());
    for round in 1..=5 {
        let id = format!("T-race-{round}");
        let inbox = format!("tasks/inbox/{id}.md");
        let (s, _) = t.machine("POST", "/api/files/commit", Some(json!({ "changes": [{ "path": inbox, "base_version": 0, "content": b64(b"task") }] }))).await;
        assert_eq!(s, 200);
        let mut joins = Vec::new();
        for h in ["h1", "h2", "h3", "h4", "h5", "h6"] {
            let (t, id, inbox) = (t.clone(), id.clone(), inbox.clone());
            joins.push(tokio::spawn(async move {
                let body = json!({ "msg": "claim", "changes": [
                    { "path": inbox, "base_version": 1, "deleted": true },
                    { "path": format!("tasks/active/{id}.md"), "base_version": 0, "content": b64(format!("claimed_by: {h}\n").as_bytes()) } ] });
                (h, t.machine("POST", "/api/files/commit", Some(body)).await.0)
            }));
        }
        let mut winners = Vec::new();
        for j in joins {
            let (h, s) = j.await.unwrap();
            assert!(s == 200 || s == 409, "{s}");
            if s == 200 {
                winners.push(h);
            }
        }
        assert_eq!(winners.len(), 1, "round {round}: {winners:?}");
        let (_, r) = t.machine("GET", "/api/files/changes?since=0", None).await;
        let active = r["files"].as_array().unwrap().iter().find(|f| f["path"] == format!("tasks/active/{id}.md")).unwrap().clone();
        assert_eq!(unb64(&active["content"]), format!("claimed_by: {}\n", winners[0]).into_bytes());
        assert_eq!(active["version"], 1);
    }
}

#[tokio::test]
async fn sign_in_rules() {
    let t = T::new();
    // before 2FA is bound nobody can sign in, and no code is issued
    let (s, r) = t.call("POST", "/api/auth/start", None, Some(json!({ "email": OWNER }))).await;
    assert_eq!((s, r["error"].as_str()), (403, Some("totp_not_setup")));
    // 2FA binding is machine-only
    let (s, _) = t.call("POST", "/api/host/2fa/setup", Some("0".repeat(64).as_str()), None).await;
    assert_eq!(s, 401);

    let (session, secret) = t.sign_in().await;
    let (s, r) = t.machine("GET", "/api/host/2fa", None).await;
    assert_eq!((s, &r["bound"]), (200, &json!(true)));

    // a session reaches the owner API, not machine-only routes
    let (s, _) = t.call("GET", "/api/host/login-code", Some(&session), None).await;
    assert_eq!(s, 403);
    let (s, r) = t.call("POST", "/api/host/join", Some(&session), Some(json!({ "host": "laptop" }))).await;
    assert_eq!((s, r["token"].as_str()), (200, Some(TOKEN)));

    // not on the allow-list: refused at once
    let (s, _) = t.call("POST", "/api/auth/start", None, Some(json!({ "email": "eve@example.com" }))).await;
    assert_eq!(s, 403);
    // one code a minute
    t.advance(61_000);
    let (s, _) = t.call("POST", "/api/auth/start", None, Some(json!({ "email": OWNER }))).await;
    assert_eq!(s, 200);
    let (s, _) = t.call("POST", "/api/auth/start", None, Some(json!({ "email": OWNER }))).await;
    assert_eq!(s, 429);
    let code = t.machine("GET", "/api/host/login-code", None).await.1["code"].as_str().unwrap().to_string();
    // a used TOTP step does not work again; a wrong TOTP keeps the email code
    let (s, r) = t.call("POST", "/api/auth/verify", None, Some(json!({ "email": OWNER, "code": code, "totp": "000000" }))).await;
    assert_eq!(s, 401, "{r}");
    t.advance(30_000);
    let (s, r) = t.call("POST", "/api/auth/verify", None, Some(json!({ "email": OWNER, "code": code, "totp": t.totp(&secret) }))).await;
    assert_eq!(s, 200, "{r}");
    // the same email code works once
    t.advance(30_000);
    let (s, _) = t.call("POST", "/api/auth/verify", None, Some(json!({ "email": OWNER, "code": code, "totp": t.totp(&secret) }))).await;
    assert_eq!(s, 401);

    // logout ends the session
    let (s, _) = t.call("POST", "/api/auth/logout", Some(&session), None).await;
    assert_eq!(s, 200);
    let (s, _) = t.call("GET", "/api/files/list?prefix=memory/", Some(&session), None).await;
    assert_eq!(s, 401);

    // revoke-sessions signs everyone out and says so in the feed
    let (s, r) = t.machine("POST", "/api/host/revoke-sessions", None).await;
    assert_eq!((s, r["revoked"].as_i64()), (200, Some(1)));
    let c = t.app.db();
    let notices: i64 = c.query_row("SELECT COUNT(*) FROM events WHERE json LIKE '%All logins revoked%'", [], |r| r.get(0)).unwrap();
    assert_eq!(notices, 1);
}

#[tokio::test]
async fn idle_sessions_expire() {
    let t = T::new();
    let (session, _) = t.sign_in().await;
    let (s, _) = t.call("GET", "/api/files/list?prefix=memory/", Some(&session), None).await;
    assert_eq!(s, 200);
    t.advance(31 * 86_400_000);
    let (s, _) = t.call("GET", "/api/files/list?prefix=memory/", Some(&session), None).await;
    assert_eq!(s, 401);
}
