//! Claiming against a real server: one machine wins, the other puts the task back, and a finished task is recorded and
//! announced. The agent is a stand-in, so this runs without a Copilot login.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use noop_agent::work::{self, Agent};
use noop_agent::Ctx;
use noop_server::app::{App, Config};

struct Fake;

impl Agent for Fake {
    fn run<'a>(&'a self, prompt: &'a str, _cwd: &'a Path) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        let answer = format!("handled: {}", prompt.lines().next().unwrap_or("").trim());
        Box::pin(async move { Ok(answer) })
    }
}

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
    let d = std::env::temp_dir().join(format!("noop-work-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("data/tasks/inbox")).unwrap();
    d
}

fn task(ctx: &Ctx, id: &str, title: &str, priority: &str) {
    let mut meta = BTreeMap::new();
    meta.insert("id".into(), id.into());
    meta.insert("title".into(), title.into());
    meta.insert("priority".into(), priority.into());
    work::write_doc(&ctx.mirror().join(format!("tasks/inbox/{id}.md")), &meta, "please do this").unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_machine_claims_and_the_other_puts_it_back() {
    let base = server().await;
    let (ha, hb) = (home("a"), home("b"));
    let a = Ctx::new(&base, "k", &ha);
    let b = Ctx::new(&base, "k", &hb);

    task(&a, "T-0001", "ship it", "high");
    noop_agent::commit(&a, "plan").await.unwrap();
    noop_agent::pull(&b).await.unwrap();
    assert!(b.mirror().join("tasks/inbox/T-0001.md").is_file(), "both machines can see the task");

    let claimed = work::claim(&a, &a.mirror().join("tasks/inbox/T-0001.md"), "host-a").await.unwrap();
    assert!(claimed.is_some(), "the first machine claims it");

    let lost = work::claim(&b, &b.mirror().join("tasks/inbox/T-0001.md"), "host-b").await.unwrap();
    assert!(lost.is_none(), "the second loses the race");
    assert!(b.mirror().join("tasks/inbox/T-0001.md").is_file(), "the loser gets its inbox copy back");
    assert!(!b.mirror().join("tasks/active/T-0001.md").exists(), "and keeps no claim");

    for h in [ha, hb] { std::fs::remove_dir_all(h).ok(); }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_task_is_recorded_and_announced() {
    let base = server().await;
    let (ha, hb) = (home("c"), home("d"));
    let a = Ctx::new(&base, "k", &ha);
    let b = Ctx::new(&base, "k", &hb);

    task(&a, "T-0002", "write the report", "normal");
    noop_agent::commit(&a, "plan").await.unwrap();

    let done = work::work_once(&b, "host-b", &Fake).await.unwrap().expect("a task ran");
    assert_eq!(done.id, "T-0002");
    assert!(done.result.contains("write the report"), "the agent saw the task title: {}", done.result);

    noop_agent::pull(&a).await.unwrap();
    let done_doc = std::fs::read_to_string(a.mirror().join("tasks/done/T-0002.md")).unwrap();
    assert!(done_doc.contains("status: done"), "{done_doc}");
    assert!(done_doc.contains("## Result"), "{done_doc}");
    assert!(!a.mirror().join("tasks/active/T-0002.md").exists());
    assert!(!a.mirror().join("tasks/inbox/T-0002.md").exists());
    let journal = std::fs::read_dir(a.mirror().join("journal")).unwrap().flatten().next().expect("a journal line");
    let text = std::fs::read_to_string(journal.path()).unwrap();
    assert!(text.contains("done T-0002"), "{text}");

    for h in [ha, hb] { std::fs::remove_dir_all(h).ok(); }
}
