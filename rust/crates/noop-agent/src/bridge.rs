//! The owner's messages reach this machine, and it answers them in the chat.
//!
//! The server queues an inbox item per owner message (`{"type":"message","event":{…}}`). This drains them in order,
//! keeping a cursor, answers each through the agent, and posts the answer back as a `notice` event, which is what the app
//! shows in the chat. Other item types belong to other parts of the fleet and are skipped here.

use std::path::PathBuf;

use crate::work::Agent;
use crate::Ctx;

/// What one drain did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Step {
    /// Owner messages answered.
    pub answered: usize,
    /// Cursor after this drain, acknowledged to the server.
    pub after: i64,
}

fn cursor_path(ctx: &Ctx) -> PathBuf {
    ctx.home.join("inbox.json")
}

fn load_cursor(ctx: &Ctx) -> i64 {
    std::fs::read_to_string(cursor_path(ctx))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("after").and_then(|a| a.as_i64()))
        .unwrap_or(0)
}

fn save_cursor(ctx: &Ctx, after: i64) -> Result<(), String> {
    std::fs::create_dir_all(&ctx.home).ok();
    std::fs::write(cursor_path(ctx), format!("{{\"after\":{after}}}")).map_err(|e| format!("cursor: {e}"))
}

/// Take the next items, answer owner messages, and acknowledge up to the last one seen.
///
/// `wait_secs` holds the request open when the inbox is empty (the server caps it at 25 s), so an idle machine asks
/// rarely instead of spinning.
pub async fn step(ctx: &Ctx, host: &str, agent: &dyn Agent, wait_secs: u64) -> Result<Step, String> {
    let mut after = load_cursor(ctx);
    let mut out = Step { answered: 0, after };
    let (code, body) = ctx.call("GET", &format!("/api/host/inbox?after={after}&wait={}", wait_secs.min(25)), None).await?;
    if code != 200 {
        return Err(format!("inbox: server answered {code}"));
    }
    for item in body.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        if let Some(seq) = item.get("seq").and_then(|v| v.as_i64()) {
            after = after.max(seq);
        }
        if item.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let text = item
            .get("event")
            .and_then(|e| e.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if text.trim().is_empty() {
            continue;
        }
        let prompt = format!(
            "The owner wrote this to the fleet:\n\n{text}\n\nAnswer the owner briefly, in two or three sentences. Start no other work."
        );
        match agent.run(&prompt, &ctx.mirror()).await {
            Ok(answer) if !answer.trim().is_empty() => {
                reply(ctx, host, answer.trim()).await?;
                out.answered += 1;
            }
            Ok(_) => journal(ctx, host, "a reply came back empty; nothing was posted"),
            // the owner is never shown a raw failure; it goes to the journal for the fleet to see
            Err(e) => journal(ctx, host, &format!("could not answer the owner: {}", first_line(&e))),
        }
    }
    save_cursor(ctx, after)?;
    out.after = after;
    Ok(out)
}

/// Post a chat message from this machine, the way the app shows an answer.
async fn reply(ctx: &Ctx, host: &str, text: &str) -> Result<(), String> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let body = serde_json::json!({
        "id": format!("{host}-{nanos}"),
        "from": host,
        "kind": "notice",
        "text": text,
    });
    let (code, _) = ctx.call("POST", "/api/events", Some(body)).await?;
    if code == 200 {
        Ok(())
    } else {
        Err(format!("reply: server answered {code}"))
    }
}

fn journal(ctx: &Ctx, host: &str, line: &str) {
    let date = crate::work::today();
    let path = ctx.mirror().join("journal").join(format!("{date}.md"));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let mut text = std::fs::read_to_string(&path).unwrap_or_else(|_| format!("# Journal {date}\n\n"));
    text.push_str(&format!("- {} `{host}` {line}\n", crate::work::hhmm()));
    std::fs::write(&path, text).ok();
}

fn first_line(s: &str) -> String {
    let l = s.lines().next().unwrap_or("").trim();
    l.chars().take(160).collect()
}
