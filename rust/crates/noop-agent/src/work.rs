//! Taking a task from the fleet and recording what came of it.
//!
//! A task is a Markdown file with front matter in `tasks/inbox/`. Claiming it is a move to `tasks/active/` committed with
//! the server's compare-and-swap, which is the lock between machines: the second machine to try loses the commit and puts
//! the file back. Finishing writes `tasks/done/`, a journal line and a feed event.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::Ctx;

/// Something that can work on a task: GitHub Copilot in the shipped build, a stand-in in tests.
pub trait Agent {
    fn run<'a>(&'a self, prompt: &'a str, cwd: &'a Path) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
}

/// A document with front matter, as every Noop file is written: `---` block, then the body.
#[derive(Debug, Clone, Default)]
pub struct Doc {
    pub meta: BTreeMap<String, String>,
    pub body: String,
}

pub fn read_doc(path: &Path) -> Result<Doc, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let Some(rest) = text.strip_prefix("---\n") else {
        return Ok(Doc { meta: BTreeMap::new(), body: text });
    };
    let Some(end) = rest.find("\n---") else {
        return Ok(Doc { meta: BTreeMap::new(), body: text });
    };
    let (head, tail) = rest.split_at(end);
    let body = tail.strip_prefix("\n---").unwrap_or("").strip_prefix('\n').unwrap_or(tail).to_string();
    let mut meta = BTreeMap::new();
    for line in head.lines() {
        if let Some((k, v)) = line.split_once(':') {
            meta.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    Ok(Doc { meta, body })
}

pub fn write_doc(path: &Path, meta: &BTreeMap<String, String>, body: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut text = String::from("---\n");
    for (k, v) in meta {
        text.push_str(&format!("{k}: {}\n", v.replace('\n', " ")));
    }
    text.push_str("---\n");
    text.push_str(body.trim_end());
    text.push('\n');
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn tasks_dir(ctx: &Ctx, kind: &str) -> PathBuf {
    ctx.mirror().join("tasks").join(kind)
}

/// Task files waiting for a machine, most important first.
pub fn inbox(ctx: &Ctx) -> Vec<PathBuf> {
    let rank = |p: &str| match p {
        "urgent" => 0,
        "high" => 1,
        "low" => 3,
        _ => 2,
    };
    let mut files: Vec<(u8, PathBuf)> = std::fs::read_dir(tasks_dir(ctx, "inbox"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("T-")))
        .map(|p| {
            let prio = read_doc(&p).ok().and_then(|d| d.meta.get("priority").cloned()).unwrap_or_default();
            (rank(prio.trim()), p)
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    files.into_iter().map(|(_, p)| p).collect()
}

/// Claim one task by moving it, or report that another machine got there first.
pub async fn claim(ctx: &Ctx, path: &Path, host: &str) -> Result<Option<PathBuf>, String> {
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };
    let doc = read_doc(path)?;
    let id = doc.meta.get("id").cloned().or_else(|| {
        path.file_stem().map(|s| s.to_string_lossy().to_string())
    }).unwrap_or_default();
    if id.is_empty() {
        return Err(format!("{}: no id", path.display()));
    }
    let dest = tasks_dir(ctx, "active").join(format!("{id}.md"));
    let mut meta = doc.meta.clone();
    meta.insert("claimed_by".into(), host.to_string());
    meta.insert("started".into(), chrono_like_now());
    write_doc(&dest, &meta, &doc.body)?;
    std::fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let msg = format!("[{host}] claim {id}");
    match crate::commit_once(ctx, &msg).await? {
        Some(_) => Ok(Some(dest)),
        None => {
            // lost the race: put the task back and drop our copy of the claim
            std::fs::remove_file(&dest).ok();
            std::fs::write(path, raw).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(None)
        }
    }
}

/// A finished task.
#[derive(Debug, Clone)]
pub struct Done {
    pub id: String,
    pub result: String,
}

/// One round: claim the next task, run the agent on it, record the outcome. `Ok(None)` means nothing was waiting.
pub async fn work_once(ctx: &Ctx, host: &str, agent: &dyn Agent) -> Result<Option<Done>, String> {
    crate::pull(ctx).await?;
    let Some(path) = inbox(ctx).into_iter().next() else { return Ok(None) };
    let Some(active) = claim(ctx, &path, host).await? else { return Ok(None) };
    let doc = read_doc(&active)?;
    let id = doc.meta.get("id").cloned().unwrap_or_else(|| "T-?".into());
    let title = doc.meta.get("title").cloned().unwrap_or_default();
    let prompt = format!("{title}\n\n{}", doc.body.trim());

    let result = match agent.run(&prompt, &ctx.mirror()).await {
        Ok(r) => r,
        Err(e) => {
            // the claim is released so the task runs again; the failure is in the feed, not lost
            std::fs::remove_file(&active).ok();
            let mut meta = doc.meta.clone();
            meta.insert("failed".into(), e.clone());
            meta.remove("claimed_by");
            meta.remove("started");
            // a task that keeps failing stops looping and asks the owner how to go on
            let failures: u32 = meta.get("failures").and_then(|n| n.parse().ok()).unwrap_or(0) + 1;
            meta.insert("failures".into(), failures.to_string());
            write_doc(&tasks_dir(ctx, "inbox").join(format!("{id}.md")), &meta, &doc.body)?;
            let msg = format!("[{host}] {id} failed: {}", first_line(&e));
            if failures >= 2 {
                let question = format!("{id} keeps failing after {failures} runs: {}", first_line(&e));
                let card = crate::decisions::open_card(
                    ctx,
                    &meta,
                    &question,
                    &["A: retry once more", "B: change the approach", "C: drop it"],
                    meta.get("agent").map(String::as_str).unwrap_or(""),
                    &format!("{}\n\n## Failure\n{e}\n", doc.body.trim()),
                )?;
                event_kind(ctx, host, "decision", &format!("{id}: {question} ({})", card.id)).await?;
            }
            crate::commit(ctx, &msg).await?;
            event(ctx, host, &msg).await?;
            return Err(e);
        }
    };

    let done = tasks_dir(ctx, "done").join(format!("{id}.md"));
    let mut meta = doc.meta.clone();
    meta.insert("status".into(), "done".into());
    meta.insert("finished".into(), chrono_like_now());
    meta.remove("claimed_by");
    write_doc(&done, &meta, &format!("{}\n\n---\n\n## Result\n\n{}\n", doc.body.trim(), result.trim()))?;
    std::fs::remove_file(&active).ok();
    // the journal line goes into the same commit, so it reaches the other machines
    journal(ctx, host, &format!("done {id}: {}", first_line(&result)));
    crate::commit(ctx, &format!("[{host}] done {id}")).await?;
    event(ctx, host, &format!("{id}: {}", first_line(&result))).await?;
    Ok(Some(Done { id, result }))
}

/// Add a line to today's journal, the fleet's running record.
fn journal(ctx: &Ctx, host: &str, line: &str) {
    let path = ctx.mirror().join("journal").join(format!("{}.md", today()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let mut text = std::fs::read_to_string(&path).unwrap_or_else(|_| format!("# Journal {}\n\n", today()));
    text.push_str(&format!("- {} `{host}` {line}\n", hhmm()));
    std::fs::write(&path, text).ok();
}

/// A feed event, so the owner sees the result without asking.
async fn event(ctx: &Ctx, host: &str, text: &str) -> Result<(), String> {
    event_kind(ctx, host, "result", text).await
}

/// A feed event of a given kind (`result`, `decision`, `notice`).
async fn event_kind(ctx: &Ctx, host: &str, kind: &str, text: &str) -> Result<(), String> {
    let body = serde_json::json!({ "from": host, "kind": kind, "text": text });
    let (code, _) = ctx.call("POST", "/api/events", Some(body)).await?;
    if code == 200 {
        Ok(())
    } else {
        Err(format!("event: server answered {code}"))
    }
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() > 160 {
        line.chars().take(160).collect::<String>() + "…"
    } else {
        line.to_string()
    }
}

// Dates in UTC, the format the fleet's files use.
fn stamp(fmt: &str) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (mut days, secs) = (now / 86_400, now % 86_400);
    let (mut y, mut m) = (1970i64, 1i64);
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let len = match m { 1 | 3 | 5 | 7 | 8 | 10 | 12 => 31, 4 | 6 | 9 | 11 => 30, _ => if leap { 29 } else { 28 } };
        if days < len { break }
        days -= len;
        m += 1;
        if m > 12 { m = 1; y += 1 }
    }
    let (h, mi) = (secs / 3600, (secs % 3600) / 60);
    match fmt {
        "date" => format!("{y:04}-{m:02}-{:02}", days + 1),
        _ => format!("{h:02}:{mi:02}"),
    }
}

fn chrono_like_now() -> String {
    format!("{}T{}:00Z", stamp("date"), stamp("time"))
}

/// Today in UTC, `YYYY-MM-DD`, as the journal files are named.
pub fn today() -> String {
    stamp("date")
}

/// Now in UTC, `HH:MM`, as journal lines are stamped.
pub fn hhmm() -> String {
    stamp("time")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_round_trip() {
        let dir = std::env::temp_dir().join(format!("noop-doc-{}", std::process::id()));
        let p = dir.join("T-1.md");
        let mut meta = BTreeMap::new();
        meta.insert("id".into(), "T-1".into());
        meta.insert("priority".into(), "high".into());
        write_doc(&p, &meta, "do the thing").unwrap();
        let d = read_doc(&p).unwrap();
        assert_eq!(d.meta.get("id").map(String::as_str), Some("T-1"));
        assert_eq!(d.body.trim(), "do the thing");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn priority_order() {
        let home = std::env::temp_dir().join(format!("noop-inbox-{}", std::process::id()));
        let ctx = Ctx::new("http://x", "k", &home);
        for (id, p) in [("T-3", "low"), ("T-1", "urgent"), ("T-2", "normal")] {
            let mut meta = BTreeMap::new();
            meta.insert("id".into(), id.to_string());
            meta.insert("priority".into(), p.to_string());
            write_doc(&tasks_dir(&ctx, "inbox").join(format!("{id}.md")), &meta, "x").unwrap();
        }
        let ids: Vec<String> = inbox(&ctx).iter().map(|p| p.file_stem().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(ids, ["T-1", "T-2", "T-3"]);
        std::fs::remove_dir_all(&home).ok();
    }
}
