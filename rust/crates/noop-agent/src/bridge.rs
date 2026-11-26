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
    /// A settings, channel or cancel item arrived, so the shared files were pulled again.
    pub files: bool,
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
    let mut out = Step { answered: 0, after, files: false };
    let (code, body) = ctx.call("GET", &format!("/api/host/inbox?after={after}&wait={}", wait_secs.min(25)), None).await?;
    if code != 200 {
        return Err(format!("inbox: server answered {code}"));
    }
    for item in body.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        if let Some(seq) = item.get("seq").and_then(|v| v.as_i64()) {
            after = after.max(seq);
        }
        let kind = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if kind != "message" {
            match kind {
                // the owner changed a setting: merge the patch into this machine's config.toml and share it
                "settings" => {
                    if let Some(patch) = item.get("patch") {
                        if apply_settings(ctx, patch)? {
                            crate::commit(ctx, "settings from the app").await?;
                            out.files = true;
                        }
                    }
                }
                // a channel edit, a fleet update or a cancel is written to the shared files: pull them in
                "channel" | "cancel" | "update" => out.files = true,
                _ => {}
            }
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
    if out.files {
        crate::pull(ctx).await?;
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

/// Merge a settings patch (JSON, nested) into `config.toml`. Returns whether anything changed.
fn apply_settings(ctx: &Ctx, patch: &serde_json::Value) -> Result<bool, String> {
    let path = ctx.mirror().join("config.toml");
    let mut doc: toml::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| t.parse::<toml::Value>().ok())
        .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
    if !merge_toml(&mut doc, patch) {
        return Ok(false);
    }
    let text = toml::to_string_pretty(&doc).map_err(|e| format!("config.toml: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("config.toml: {e}"))?;
    Ok(true)
}

/// Overlay JSON onto TOML, recursing into tables; returns whether a value actually changed.
fn merge_toml(dst: &mut toml::Value, patch: &serde_json::Value) -> bool {
    let Some(obj) = patch.as_object() else { return false };
    if !dst.is_table() {
        *dst = toml::Value::Table(toml::map::Map::new());
    }
    let table = dst.as_table_mut().expect("a table");
    let mut changed = false;
    for (k, v) in obj {
        if v.is_null() {
            changed |= table.remove(k).is_some();
            continue;
        }
        if v.is_object() {
            let entry = table.entry(k.clone()).or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            changed |= merge_toml(entry, v);
            continue;
        }
        let to = match v {
            serde_json::Value::String(s) => Some(toml::Value::String(s.clone())),
            serde_json::Value::Bool(b) => Some(toml::Value::Boolean(*b)),
            serde_json::Value::Number(n) => n.as_i64().map(toml::Value::Integer).or_else(|| n.as_f64().map(toml::Value::Float)),
            serde_json::Value::Array(a) => {
                let items: Option<Vec<toml::Value>> = a
                    .iter()
                    .map(|x| match x {
                        serde_json::Value::String(s) => Some(toml::Value::String(s.clone())),
                        serde_json::Value::Bool(b) => Some(toml::Value::Boolean(*b)),
                        serde_json::Value::Number(n) => n.as_i64().map(toml::Value::Integer),
                        _ => None,
                    })
                    .collect();
                items.map(toml::Value::Array)
            }
            _ => None,
        };
        if let Some(to) = to {
            if table.get(k) != Some(&to) {
                table.insert(k.clone(), to);
                changed = true;
            }
        }
    }
    changed
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
