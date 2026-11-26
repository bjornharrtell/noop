//! Decision cards, without a server: opening one parks its task, answering puts it back with the answer, and a precedent
//! settles the next identical question without the owner.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use noop_agent::decisions::{self, Judgement};
use noop_agent::work::{read_doc, write_doc, Agent};
use noop_agent::Ctx;

fn home(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("noop-dec-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("data/tasks/inbox")).unwrap();
    d
}

fn queued(ctx: &Ctx, id: &str, title: &str) -> BTreeMap<String, String> {
    let mut meta = BTreeMap::new();
    meta.insert("id".into(), id.into());
    meta.insert("title".into(), title.into());
    meta.insert("priority".into(), "normal".into());
    write_doc(&ctx.mirror().join(format!("tasks/inbox/{id}.md")), &meta, "do the thing").unwrap();
    meta
}

#[test]
fn a_card_parks_its_task_and_an_answer_brings_it_back() {
    let h = home("a");
    let ctx = Ctx::new("http://x", "k", &h);
    let meta = queued(&ctx, "T-0001", "ship it");

    let card = decisions::open_card(&ctx, &meta, "Merge the PR?", &["A: approve", "B: reject"], "lead", "the gate").unwrap();
    assert_eq!(card.id, "D-0001");
    assert!(ctx.mirror().join("decisions/open/D-0001.md").is_file());
    assert!(ctx.mirror().join("tasks/blocked/T-0001.md").is_file(), "the task waits");
    assert!(!ctx.mirror().join("tasks/inbox/T-0001.md").exists());

    // asking again for the same task refreshes the card instead of opening a second one
    let again = decisions::open_card(&ctx, &meta, "Merge the PR?", &["A: approve", "B: reject"], "lead", "again").unwrap();
    assert_eq!(again.id, "D-0001");
    assert_eq!(decisions::open(&ctx).len(), 1);

    assert!(decisions::answer(&ctx, "D-0001", "approve", "owner (root)", "").unwrap());
    assert!(ctx.mirror().join("decisions/closed/D-0001.md").is_file());
    assert!(ctx.mirror().join("tasks/inbox/T-0001.md").is_file(), "the task resumes");
    assert!(!ctx.mirror().join("tasks/blocked/T-0001.md").exists());
    let task = read_doc(&ctx.mirror().join("tasks/inbox/T-0001.md")).unwrap();
    assert!(task.body.contains("A: **approve**"), "the answer is in the task: {}", task.body);
    assert_eq!(task.meta.get("resumes").map(String::as_str), Some("D-0001"));

    std::fs::remove_dir_all(h).ok();
}

struct Fixed(&'static str);

impl Agent for Fixed {
    fn run<'a>(&'a self, _p: &'a str, _c: &'a Path) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        let out = self.0;
        Box::pin(async move { Ok(out.to_string()) })
    }
}

#[tokio::test]
async fn a_precedent_settles_a_repeat_question() {
    let h = home("b");
    let ctx = Ctx::new("http://x", "k", &h);

    // the owner once answered "Merge the PR?" with approve
    let meta = queued(&ctx, "T-0001", "ship it");
    let first = decisions::open_card(&ctx, &meta, "Merge the PR?", &["A: approve", "B: reject"], "lead", "gate").unwrap();
    assert!(decisions::answer(&ctx, &first.id, "A: approve", "owner (root)", "").unwrap(), "the answer closes the card");
    assert_eq!(decisions::closed(&ctx).len(), 1, "one closed card");
    assert_eq!(decisions::closed(&ctx)[0].meta.get("answered_by").map(String::as_str), Some("owner (root)"));

    // the same question comes back on another task
    let meta2 = queued(&ctx, "T-0002", "ship it too");
    let second = decisions::open_card(&ctx, &meta2, "Merge the PR?", &["A: approve", "B: reject"], "lead", "gate").unwrap();
    let judge = Fixed(r#"{"settled": true, "answer": "A: approve", "why": "the owner approved this exact question before"}"#);
    match decisions::judge(&ctx, &second.id, &judge).await.unwrap() {
        Judgement::Precedent(answer) => assert_eq!(answer, "A: approve"),
        other => panic!("expected a precedent, got {other:?}"),
    }
    let closed = read_doc(&ctx.mirror().join(format!("decisions/closed/{}.md", second.id))).unwrap();
    assert_eq!(closed.meta.get("answered_by").map(String::as_str), Some("system (precedent)"));
    assert!(ctx.mirror().join("tasks/inbox/T-0002.md").is_file(), "the second task resumes");

    std::fs::remove_dir_all(h).ok();
}

#[tokio::test]
async fn an_unsettled_question_waits_for_the_owner() {
    let h = home("c");
    let ctx = Ctx::new("http://x", "k", &h);
    let meta = queued(&ctx, "T-0003", "something new");
    let card = decisions::open_card(&ctx, &meta, "Delete the old cluster?", &["A: yes", "B: no"], "lead", "gate").unwrap();

    // no precedents at all
    let judge = Fixed(r#"{"settled": true, "answer": "A: yes"}"#);
    assert_eq!(decisions::judge(&ctx, &card.id, &judge).await.unwrap(), Judgement::AskOwner);

    // one precedent, but the answer is not one of the card's options
    let other = queued(&ctx, "T-0004", "x");
    let prior = decisions::open_card(&ctx, &other, "Something else?", &["A: yes", "B: no"], "lead", "").unwrap();
    decisions::answer(&ctx, &prior.id, "A: yes", "owner (root)", "").unwrap();
    let stray = Fixed(r#"{"settled": true, "answer": "C: maybe"}"#);
    assert_eq!(decisions::judge(&ctx, &card.id, &stray).await.unwrap(), Judgement::AskOwner);
    assert!(ctx.mirror().join(format!("decisions/open/{}.md", card.id)).is_file(), "still open");

    std::fs::remove_dir_all(h).ok();
}
