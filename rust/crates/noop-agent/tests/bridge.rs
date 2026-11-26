//! The inbox bridge against a real server: sign in as the owner, send a chat message, and check that the machine answers
//! it in the feed. The agent is a stand-in, so this runs without Copilot.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use noop_agent::work::Agent;
use noop_agent::{bridge, Ctx};
use noop_server::app::{App, Config};
use serde_json::json;

struct Fake;

impl Agent for Fake {
    fn run<'a>(&'a self, prompt: &'a str, _cwd: &'a Path) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        let saw = prompt.contains("check the deploy");
        Box::pin(async move { Ok(if saw { "On it: checking the deploy now.".into() } else { "hello".into() }) })
    }
}

async fn server() -> String {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let cfg = Config { hub_token: "k".into(), owners: vec!["owner@example.com".into()], en: true, ..Default::default() };
    let app = App::new(conn, cfg, None).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(noop_server::serve_on(app, l));
    base
}

fn home(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("noop-bridge-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("data")).unwrap();
    d
}

/// Sign in the way a person does: bind 2FA with the machine key, then code + TOTP.
async fn owner_session(ctx: &Ctx) -> String {
    let (_, setup) = ctx.call("POST", "/api/host/2fa/setup", None).await.unwrap();
    let secret = setup["secret"].as_str().unwrap().to_string();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let key = noop_core::totp::b32decode(&secret).unwrap();
    let step = noop_core::totp::step_at(now);
    let totp = noop_core::totp::hotp(&key, step as u64);
    let (c, _) = ctx.call("POST", "/api/host/2fa/confirm", Some(json!({ "totp": totp }))).await.unwrap();
    assert_eq!(c, 200, "2FA bound");
    // a code works once, so signing in uses the next step, as a real authenticator would
    let totp = noop_core::totp::hotp(&key, (step + 1) as u64);

    let (c, b) = ctx.call("POST", "/api/auth/start", Some(json!({ "email": "owner@example.com" }))).await.unwrap();
    assert_eq!((c, b["delivery"].as_str()), (200, Some("cli")), "without SMTP the code goes to the machine");
    let (_, lc) = ctx.call("GET", "/api/host/login-code", None).await.unwrap();
    let code = lc["code"].as_str().expect("a pending code").to_string();
    let (c, v) = ctx
        .call("POST", "/api/auth/verify", Some(json!({ "email": "owner@example.com", "code": code, "totp": totp })))
        .await
        .unwrap();
    assert_eq!(c, 200, "signed in: {v}");
    v["session"].as_str().expect("a session").to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_owner_message_is_answered_in_the_feed() {
    let base = server().await;
    let h = home("a");
    let ctx = Ctx::new(&base, "k", &h);
    let session = owner_session(&ctx).await;

    let http = reqwest::Client::new();
    let r = http
        .post(format!("{base}/api/messages"))
        .bearer_auth(&session)
        .json(&json!({ "text": "check the deploy please" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "owner message accepted");

    let step = bridge::step(&ctx, "host-a", &Fake, 0).await.unwrap();
    assert_eq!(step.answered, 1, "one message answered");
    assert!(step.after > 0, "cursor moved: {step:?}");

    // the owner sees the answer in the chat
    let feed: serde_json::Value = http
        .get(format!("{base}/api/feed?since="))
        .bearer_auth(&session)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let texts: Vec<String> = feed["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["text"].as_str().map(str::to_string))
        .collect();
    assert!(texts.iter().any(|t| t.contains("checking the deploy")), "{texts:?}");

    // a second drain has nothing left and does not answer twice
    let again = bridge::step(&ctx, "host-a", &Fake, 0).await.unwrap();
    assert_eq!(again.answered, 0);

    std::fs::remove_dir_all(h).ok();
}

/// The assistant may create a task instead of only answering: that is how "please fix X" becomes work.
struct Decides(&'static str);

impl Agent for Decides {
    fn run<'a>(&'a self, _p: &'a str, _c: &'a Path) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        let out = self.0;
        Box::pin(async move { Ok(out.to_string()) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_can_become_a_task() {
    let base = server().await;
    let h = home("task");
    let ctx = Ctx::new(&base, "k", &h);
    let session = owner_session(&ctx).await;

    let http = reqwest::Client::new();
    let r = http
        .post(format!("{base}/api/messages"))
        .bearer_auth(&session)
        .json(&json!({ "text": "please look into the flaky login test" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);

    let answer = r#"{"action":"task","title":"Fix the flaky login test","details":"It fails on CI about one run in three; find the race.","priority":"high","text":"On it: looking at the login test."}"#;
    let step = bridge::step(&ctx, "host-a", &Decides(answer), 0).await.unwrap();
    assert_eq!(step.answered, 1);

    // the task is in the mirror, with the fields the fleet reads
    let path = ctx.mirror().join("tasks/inbox/T-0001.md");
    let doc = noop_agent::work::read_doc(&path).expect("the task exists");
    assert_eq!(doc.meta.get("title").map(String::as_str), Some("Fix the flaky login test"));
    assert_eq!(doc.meta.get("priority").map(String::as_str), Some("high"));
    assert_eq!(doc.meta.get("source").map(String::as_str), Some("owner"));
    assert_eq!(doc.meta.get("agent").map(String::as_str), Some("lead"));
    assert!(doc.body.contains("about one run in three"), "{}", doc.body);

    // it was pushed, and the owner was told in the chat
    noop_agent::pull(&ctx).await.unwrap();
    let feed: serde_json::Value = http
        .get(format!("{base}/api/feed?since="))
        .bearer_auth(&session)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let texts: Vec<String> = feed["events"].as_array().unwrap().iter().filter_map(|e| e["text"].as_str().map(str::to_string)).collect();
    assert!(texts.iter().any(|t| t.contains("looking at the login test")), "{texts:?}");

    std::fs::remove_dir_all(h).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_question_is_only_answered() {
    let base = server().await;
    let h = home("ask");
    let ctx = Ctx::new(&base, "k", &h);
    let session = owner_session(&ctx).await;
    let http = reqwest::Client::new();
    http.post(format!("{base}/api/messages"))
        .bearer_auth(&session)
        .json(&json!({ "text": "how many tasks are queued?" }))
        .send()
        .await
        .unwrap();

    let answer = r#"{"action":"reply","text":"Nothing is queued right now."}"#;
    let step = bridge::step(&ctx, "host-a", &Decides(answer), 0).await.unwrap();
    assert_eq!(step.answered, 1);
    assert!(!ctx.mirror().join("tasks/inbox").read_dir().map(|mut d| d.next().is_some()).unwrap_or(false), "no task was created");

    std::fs::remove_dir_all(h).ok();
}
