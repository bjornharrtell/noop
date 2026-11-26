//! A due routine becomes exactly one task — even when two machines tick in the same second — and a disabled routine
//! never fires.

use std::path::PathBuf;

use noop_agent::routines;
use noop_agent::work::read_doc;
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

fn home(tag: &str, config: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("noop-routines-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("data/tasks/inbox")).unwrap();
    std::fs::write(d.join("data/config.toml"), config).unwrap();
    d
}

const CONFIG: &str = r#"
[[routine]]
name = "weekly-report"
every = "daily 09:00"
scheduler = "each"
agent = "reporter"
priority = "high"
title = "weekly report"
prompt = "summarise the week for the owner"
"#;

/// 2026-11-26 09:30 UTC: half an hour after the slot.
const NOW: i64 = 498_802 * 3_600_000 - 30 * 60_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_due_routine_is_queued_once() {
    let base = server().await;
    let h = home("a", CONFIG);
    let ctx = Ctx::new(&base, "k", &h);

    assert_eq!(routines::queue(&ctx, "host-a", NOW).await.unwrap(), 1, "the routine fires");
    let doc = read_doc(&ctx.mirror().join("tasks/inbox/T-0001.md")).unwrap();
    assert_eq!(doc.meta.get("source").map(String::as_str), Some("routine:weekly-report"));
    assert_eq!(doc.meta.get("priority").map(String::as_str), Some("high"));
    assert_eq!(doc.meta.get("agent").map(String::as_str), Some("reporter"));
    assert!(doc.meta.get("title").is_some_and(|t| t.contains("host-a")), "{:?}", doc.meta);
    assert_eq!(doc.body.trim(), "summarise the week for the owner");

    assert_eq!(routines::queue(&ctx, "host-a", NOW + 60_000).await.unwrap(), 0, "not twice in one day");
    assert_eq!(routines::queue(&ctx, "host-a", NOW + 3_600_000).await.unwrap(), 0, "still the same day");

    std::fs::remove_dir_all(h).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_machines_do_not_queue_it_twice() {
    let base = server().await;
    let (ha, hb) = (home("b", CONFIG), home("c", CONFIG));
    let a = Ctx::new(&base, "k", &ha);
    let b = Ctx::new(&base, "k", &hb);
    noop_agent::pull(&b).await.unwrap();

    let (ra, rb) = tokio::join!(routines::queue(&a, "host-a", NOW), routines::queue(&b, "host-b", NOW));
    assert_eq!(ra.unwrap() + rb.unwrap(), 1, "exactly one machine queues it");

    // and the other machine sees the one task, not two
    noop_agent::pull(&b).await.unwrap();
    let mut seen = 0;
    if let Ok(rd) = std::fs::read_dir(b.mirror().join("tasks/inbox")) {
        for e in rd.flatten() {
            if let Ok(doc) = read_doc(&e.path()) {
                if doc.meta.get("source").map(String::as_str) == Some("routine:weekly-report") {
                    seen += 1;
                }
            }
        }
    }
    assert_eq!(seen, 1, "one routine task in the fleet");

    for h in [ha, hb] {
        std::fs::remove_dir_all(h).ok();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_routine_never_fires() {
    let base = server().await;
    let h = home("d", &format!("{CONFIG}enabled = false\n"));
    let ctx = Ctx::new(&base, "k", &h);
    assert_eq!(routines::queue(&ctx, "host-a", NOW).await.unwrap(), 0);
    std::fs::remove_dir_all(h).ok();
}
