//! A due routine becomes exactly one task, and its copy is recognised so the next tick does not queue it again.

use std::path::PathBuf;

use noop_agent::routines;
use noop_agent::work::read_doc;
use noop_agent::Ctx;

fn home(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("noop-routines-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("data/tasks/inbox")).unwrap();
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

#[test]
fn a_due_routine_is_queued_once_per_period() {
    let h = home("a");
    let ctx = Ctx::new("http://x", "k", &h);
    std::fs::write(ctx.mirror().join("config.toml"), CONFIG).unwrap();
    // 2026-11-26 09:30 UTC: half an hour after the slot
    let now = 498_802 * 3_600_000 - 30 * 60_000;

    assert_eq!(routines::queue(&ctx, "host-a", now).unwrap(), 1, "the routine fires");
    let doc = read_doc(&ctx.mirror().join("tasks/inbox/T-0001.md")).unwrap();
    assert_eq!(doc.meta.get("source").map(String::as_str), Some("routine:weekly-report"));
    assert_eq!(doc.meta.get("priority").map(String::as_str), Some("high"));
    assert_eq!(doc.meta.get("agent").map(String::as_str), Some("reporter"));
    assert!(doc.meta.get("title").is_some_and(|t| t.contains("host-a")), "{:?}", doc.meta);
    assert_eq!(doc.body.trim(), "summarise the week for the owner");

    assert_eq!(routines::queue(&ctx, "host-a", now + 60_000).unwrap(), 0, "not twice in one day");
    assert_eq!(routines::queue(&ctx, "host-a", now + 3_600_000).unwrap(), 0, "still the same day");
    assert_eq!(routines::queue(&ctx, "host-a", now + 24 * 3_600_000).unwrap(), 0, "a copy is still queued");

    std::fs::remove_dir_all(h).ok();
}

#[test]
fn a_disabled_routine_never_fires() {
    let h = home("b");
    let ctx = Ctx::new("http://x", "k", &h);
    let cfg = format!("{CONFIG}enabled = false\n");
    std::fs::write(ctx.mirror().join("config.toml"), cfg).unwrap();
    let now = 498_802 * 3_600_000 - 30 * 60_000;
    assert_eq!(routines::queue(&ctx, "host-a", now).unwrap(), 0);
    std::fs::remove_dir_all(h).ok();
}
