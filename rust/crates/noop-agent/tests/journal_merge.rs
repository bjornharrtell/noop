//! Two machines writing the same day's journal: both lines survive, with no conflict markers, and the merged file
//! reaches the server.

use std::path::PathBuf;

use noop_agent::Ctx;
use noop_server::app::{App, Config};

async fn server() -> String {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let cfg = Config { hub_token: "k".into(), owners: vec![], en: true, ..Default::default() };
    let app = App::new(conn, cfg, None).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(noop_server::serve_on(app, l));
    base
}

fn home(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("noop-merge-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("data/journal")).unwrap();
    d
}

fn append(ctx: &Ctx, day: &str, line: &str) {
    let p = ctx.mirror().join(format!("journal/{day}.md"));
    let mut text = std::fs::read_to_string(&p).unwrap_or_else(|_| format!("# Journal {day}\n\n"));
    text.push_str(&format!("- {line}\n"));
    std::fs::write(p, text).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_machines_lines_survive() {
    let base = server().await;
    let (ha, hb) = (home("a"), home("b"));
    let a = Ctx::new(&base, "k", &ha);
    let b = Ctx::new(&base, "k", &hb);
    let day = "2026-11-26";

    append(&a, day, "09:00 `host-a` claimed T-0001");
    noop_agent::commit(&a, "a writes").await.unwrap();
    noop_agent::pull(&b).await.unwrap();

    // both add their own line; b's first commit is rejected, so it pulls and merges
    append(&a, day, "09:05 `host-a` finished T-0001");
    noop_agent::commit(&a, "a writes again").await.unwrap();
    append(&b, day, "09:06 `host-b` claimed T-0002");
    let c = noop_agent::commit(&b, "b writes").await.unwrap();
    assert_eq!(c.pushed, 1, "the merged journal was pushed");

    noop_agent::pull(&a).await.unwrap();
    let text = std::fs::read_to_string(a.mirror().join(format!("journal/{day}.md"))).unwrap();
    assert!(text.contains("host-a` finished T-0001"), "{text}");
    assert!(text.contains("host-b` claimed T-0002"), "{text}");
    assert_eq!(text.matches("# Journal").count(), 1, "one header: {text}");
    for marker in ["<<<<<<<", "=======", ">>>>>>>"] {
        assert!(!text.contains(marker), "no conflict markers: {text}");
    }

    // and the same text is what the server now holds for the other machine
    noop_agent::pull(&b).await.unwrap();
    let on_b = std::fs::read_to_string(b.mirror().join(format!("journal/{day}.md"))).unwrap();
    assert!(on_b.contains("host-a` finished T-0001") && on_b.contains("host-b` claimed T-0002"), "{on_b}");

    for h in [ha, hb] {
        std::fs::remove_dir_all(h).ok();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settings_change_pulls_the_shared_files() {
    use noop_agent::bridge;
    use noop_agent::work::Agent;
    use serde_json::json;

    struct Never;
    impl Agent for Never {
        fn run<'a>(&'a self, _p: &'a str, _c: &'a std::path::Path) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async { Err("no messages expected".into()) })
        }
    }

    let base = server().await;
    let h = home("c");
    let ctx = Ctx::new(&base, "k", &h);

    // the owner changes a setting through the app: the server writes config.toml and queues an item
    let http = reqwest::Client::new();
    let (_, setup) = ctx.call("POST", "/api/host/2fa/setup", None).await.unwrap();
    let secret = setup["secret"].as_str().unwrap().to_string();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let key = noop_core::totp::b32decode(&secret).unwrap();
    let step = noop_core::totp::step_at(now);
    ctx.call("POST", "/api/host/2fa/confirm", Some(json!({ "totp": noop_core::totp::hotp(&key, step as u64) }))).await.unwrap();
    let email = "owner@example.com";
    ctx.call("POST", "/api/auth/start", Some(json!({ "email": email }))).await.ok();
    let (_, lc) = ctx.call("GET", "/api/host/login-code", None).await.unwrap();
    let code = lc["code"].as_str().unwrap_or("").to_string();
    let (c, v) = ctx
        .call("POST", "/api/auth/verify", Some(json!({ "email": email, "code": code, "totp": noop_core::totp::hotp(&key, (step + 1) as u64) })))
        .await
        .unwrap();
    assert_eq!(c, 200, "signed in: {v}");
    let session = v["session"].as_str().unwrap().to_string();
    let r = http.put(format!("{base}/api/settings")).bearer_auth(&session).json(&json!({ "patch": { "fleet": { "plan_wait_minutes": 7 } } })).send().await.unwrap();
    assert!(r.status().is_success(), "settings saved: {}", r.status());

    let step = bridge::step(&ctx, "host-c", &Never, 0).await.unwrap();
    assert!(step.files, "the item means the files changed: {step:?}");
    let cfg = std::fs::read_to_string(ctx.mirror().join("config.toml")).unwrap_or_default();
    assert!(cfg.contains("plan_wait_minutes"), "config was pulled: {cfg}");

    std::fs::remove_dir_all(h).ok();
}
