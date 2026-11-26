//! Sync against a real server (in-process, in-memory database): mirror a file, push, pull into a second machine, and let
//! two machines race for one file the way task claiming does.

use std::path::PathBuf;

use noop_agent::{commit, pull, Ctx};
use noop_server::app::{App, Config};

async fn server() -> String {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let cfg = Config { hub_token: "machine-key".into(), owners: vec![], en: true, ..Default::default() };
    let app = App::new(conn, cfg, None).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(noop_server::serve_on(app, l));
    base
}

fn home(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("noop-agent-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("data")).unwrap();
    d
}

fn write(ctx: &Ctx, rel: &str, text: &str) {
    let p = ctx.mirror().join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn read(ctx: &Ctx, rel: &str) -> String {
    std::fs::read_to_string(ctx.mirror().join(rel)).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_mirrors_between_two_machines() {
    let base = server().await;
    let (ha, hb) = (home("a"), home("b"));
    let a = Ctx::new(&base, "machine-key", &ha);
    let b = Ctx::new(&base, "machine-key", &hb);

    write(&a, "memory/project.md", "first");
    let c = commit(&a, "first").await.unwrap();
    assert_eq!(c.pushed, 1);

    let p = pull(&b).await.unwrap();
    assert_eq!(p.written, 1);
    assert_eq!(read(&b, "memory/project.md"), "first");

    // the second machine edits and pushes; the first pulls that edit back
    write(&b, "memory/project.md", "second");
    assert_eq!(commit(&b, "second").await.unwrap().pushed, 1);
    pull(&a).await.unwrap();
    assert_eq!(read(&a, "memory/project.md"), "second");

    // nothing left unsent on either side, and the mirror records where it stands
    assert_eq!(commit(&a, "noop").await.unwrap().pushed, 0);
    let st = std::fs::read_to_string(ha.join("sync.json")).unwrap();
    assert!(st.contains("memory/project.md"), "{st}");

    // a file removed on one machine is removed on the other
    std::fs::remove_file(b.mirror().join("memory/project.md")).unwrap();
    assert_eq!(commit(&b, "delete").await.unwrap().pushed, 1);
    pull(&a).await.unwrap();
    assert!(!a.mirror().join("memory/project.md").exists());

    for h in [ha, hb] {
        std::fs::remove_dir_all(h).ok();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_edit_is_retried_instead_of_rejected() {
    let base = server().await;
    let (ha, hb) = (home("c"), home("d"));
    let a = Ctx::new(&base, "machine-key", &ha);
    let b = Ctx::new(&base, "machine-key", &hb);

    write(&a, "tasks/inbox/T-1.md", "claimed by a");
    commit(&a, "claim").await.unwrap();
    pull(&b).await.unwrap();

    // a writes again, so b's next push is against a version it has never seen
    write(&a, "tasks/inbox/T-1.md", "a again");
    commit(&a, "a again").await.unwrap();

    write(&b, "tasks/inbox/T-1.md", "b wins");
    let c = commit(&b, "b edit").await.unwrap();
    assert_eq!(c.pushed, 1, "the conflict was retried, not dropped");

    pull(&a).await.unwrap();
    assert_eq!(read(&a, "tasks/inbox/T-1.md"), "b wins");

    // every write got its own server version (1, 2, 3), so nothing was silently dropped: /api/files/history is the
    // owner's view
    let st: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(hb.join("sync.json")).unwrap()).unwrap();
    assert_eq!(st["files"]["tasks/inbox/T-1.md"]["version"], 3, "{st}");

    for h in [ha, hb] {
        std::fs::remove_dir_all(h).ok();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_key_is_refused() {
    let base = server().await;
    let h = home("e");
    let ctx = Ctx::new(&base, "wrong", &h);
    let (code, _) = ctx.call("GET", "/api/status", None).await.unwrap();
    assert_eq!(code, 401);
    std::fs::remove_dir_all(h).ok();
}
