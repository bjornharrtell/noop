//! Decisions: an agent asks, the fleet either follows a precedent or asks the owner.
//!
//! Ported from src/decisions.ts. A card is `decisions/open/D-####.md`; its task waits in `tasks/blocked/`. Answering
//! closes the card into `decisions/closed/` and puts the task back in the inbox with the answer attached, so the next run
//! continues with the owner's choice. A precedent — a closed card with the same question and the same answer — settles a
//! repeat question without bothering the owner, which is what the TypeScript client's judge did.

use std::collections::BTreeMap;

use crate::work::{read_doc, write_doc};
use crate::Ctx;

/// A decision card.
#[derive(Debug, Clone, Default)]
pub struct Card {
    pub id: String,
    pub task: String,
    pub question: String,
    pub options: String,
    pub agent: String,
    pub meta: BTreeMap<String, String>,
    pub body: String,
}

fn dir(ctx: &Ctx, kind: &str) -> std::path::PathBuf {
    ctx.mirror().join("decisions").join(kind)
}

fn cards(ctx: &Ctx, kind: &str) -> Vec<Card> {
    let Ok(rd) = std::fs::read_dir(dir(ctx, kind)) else { return Vec::new() };
    let mut out: Vec<Card> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .filter_map(|p| {
            let doc = read_doc(&p).ok()?;
            Some(Card {
                id: doc.meta.get("id").cloned().unwrap_or_else(|| p.file_stem().unwrap_or_default().to_string_lossy().to_string()),
                task: doc.meta.get("task").cloned().unwrap_or_default(),
                question: doc.meta.get("question").cloned().unwrap_or_default(),
                options: doc.meta.get("options").map(|s| s.split(" | ").collect::<Vec<_>>().join("\n")).unwrap_or_default(),
                agent: doc.meta.get("agent").cloned().unwrap_or_default(),
                meta: doc.meta,
                body: doc.body,
            })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Cards waiting for the owner.
pub fn open(ctx: &Ctx) -> Vec<Card> {
    cards(ctx, "open")
}

/// Answered and superseded cards, newest first by id.
pub fn closed(ctx: &Ctx) -> Vec<Card> {
    cards(ctx, "closed")
}

/// `D-0001` style, after the highest existing card of either state.
pub fn next_id(ctx: &Ctx) -> String {
    let mut max = 0u32;
    for kind in ["open", "closed"] {
        let Ok(rd) = std::fs::read_dir(dir(ctx, kind)) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(n) = name.strip_prefix("D-").and_then(|n| n.trim_end_matches(".md").parse::<u32>().ok()) {
                max = max.max(n);
            }
        }
    }
    format!("D-{:04}", max + 1)
}

/// The same question differs between runs only in spacing and a trailing mark.
#[allow(dead_code)]
fn question_key(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .trim_end_matches(['?', '？', '!', '！', '。', '.'])
        .to_string()
}

/// Open a card for a task that cannot go on without the owner. One open card per task: a re-run refreshes it instead of
/// adding a second one (the TypeScript client learned this the hard way with D-0007 and D-0008).
pub fn open_card(
    ctx: &Ctx,
    task: &BTreeMap<String, String>,
    question: &str,
    options: &[&str],
    agent: &str,
    body: &str,
) -> Result<Card, String> {
    let task_id = task.get("id").cloned().unwrap_or_default();
    if let Some(mut existing) = open(ctx).into_iter().find(|c| c.task == task_id) {
        existing.question = question.to_string();
        existing.options = if options.is_empty() { "A: approve\nB: reject".into() } else { options.join("\n") };
        existing.body = body.to_string();
        write_card(ctx, &existing)?;
        return Ok(existing);
    }
    let card = Card {
        id: next_id(ctx),
        task: task_id.clone(),
        question: question.to_string(),
        options: if options.is_empty() { "A: approve\nB: reject".into() } else { options.join("\n") },
        agent: agent.to_string(),
        meta: BTreeMap::new(),
        body: body.to_string(),
    };
    write_card(ctx, &card)?;
    // the task waits for the answer instead of being retried in a loop
    let from = ctx.mirror().join(format!("tasks/inbox/{task_id}.md"));
    if from.is_file() {
        let mut meta = task.clone();
        meta.remove("claimed_by");
        meta.remove("started");
        meta.insert("blocked_on".into(), card.id.clone());
        write_doc(&ctx.mirror().join(format!("tasks/blocked/{task_id}.md")), &meta, body)?;
        std::fs::remove_file(&from).map_err(|e| format!("{}: {e}", from.display()))?;
    }
    Ok(card)
}

fn write_card(ctx: &Ctx, card: &Card) -> Result<(), String> {
    let mut meta = card.meta.clone();
    meta.insert("id".into(), card.id.clone());
    meta.insert("task".into(), card.task.clone());
    meta.insert("question".into(), card.question.clone());
    meta.insert("options".into(), card.options.replace('\n', " | "));
    if !card.agent.is_empty() {
        meta.insert("agent".into(), card.agent.clone());
    }
    write_doc(&dir(ctx, "open").join(format!("{}.md", card.id)), &meta, &card.body)
}

/// Answer a card: close it and put its task back in the inbox with the answer.
pub fn answer(ctx: &Ctx, id: &str, answer: &str, by: &str, note: &str) -> Result<bool, String> {
    let path = dir(ctx, "open").join(format!("{id}.md"));
    if !path.is_file() {
        return Ok(false);
    }
    let doc = read_doc(&path)?;
    let mut meta = doc.meta.clone();
    meta.insert("answer".into(), answer.to_string());
    meta.insert("answered_by".into(), by.to_string());
    meta.insert("answered".into(), crate::work::today());
    let body = format!("{}\n## Answer ({by})\n{answer}\n{}", doc.body.trim_end(), if note.is_empty() { String::new() } else { format!("{note}\n") });
    write_doc(&dir(ctx, "closed").join(format!("{id}.md")), &meta, &body)?;
    std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;

    let task = doc.meta.get("task").cloned().unwrap_or_default();
    let blocked = ctx.mirror().join(format!("tasks/blocked/{task}.md"));
    if blocked.is_file() {
        let tdoc = read_doc(&blocked)?;
        let mut tmeta = tdoc.meta.clone();
        tmeta.remove("blocked_on");
        tmeta.insert("resumes".into(), id.to_string());
        let tbody = format!(
            "{}\n\n## Decision {id} ({by})\nQ: {}\nA: **{answer}**\nContinue the task accordingly.\n",
            tdoc.body.trim_end(),
            doc.meta.get("question").cloned().unwrap_or_default()
        );
        write_doc(&ctx.mirror().join(format!("tasks/inbox/{task}.md")), &tmeta, &tbody)?;
        std::fs::remove_file(&blocked).map_err(|e| format!("{}: {e}", blocked.display()))?;
    }
    Ok(true)
}

/// What the judge decided.
#[derive(Debug, PartialEq)]
pub enum Judgement {
    /// A precedent covers it: the card was closed with that answer.
    Precedent(String),
    /// Nothing settles it, so the owner is asked.
    AskOwner,
}

/// Answer the card from a precedent, or leave it for the owner.
///
/// The agent is asked with the closed cards as precedents, and must answer with a small JSON object. Only an exact,
/// unambiguous match counts; anything else goes to the owner, because a wrong automatic answer is worse than a question.
pub async fn judge(ctx: &Ctx, id: &str, agent: &dyn crate::work::Agent) -> Result<Judgement, String> {
    let card = match open(ctx).into_iter().find(|c| c.id == id) {
        Some(c) => c,
        None => return Err(format!("{id} is not open")),
    };
    let precedents = closed(ctx)
        .into_iter()
        .rev()
        .take(20)
        .filter(|c| c.meta.get("answered_by").map(|b| b.starts_with("owner")).unwrap_or(false))
        .map(|c| format!("- Q: {}\n  A: {}", c.question, c.meta.get("answer").cloned().unwrap_or_default()))
        .collect::<Vec<_>>()
        .join("\n");
    if precedents.is_empty() {
        return Ok(Judgement::AskOwner);
    }
    let prompt = format!(
        "You decide whether the owner's earlier answers settle a new question, so the owner is not asked twice.\n\nPrecedents (question, then the owner's answer):\n{precedents}\n\nNew question: {}\nOptions:\n{}\n\nReply with one JSON object and nothing else:\n{{\"settled\": true, \"answer\": \"<one of the options, copied exactly>\", \"why\": \"<one line>\"}}\nor {{\"settled\": false}}.\nOnly say settled when a precedent covers this question exactly; a different question about the same subject is not settled.",
        card.question, card.options
    );
    let reply = agent.run(&prompt, &ctx.mirror()).await?;
    let json = reply
        .find('{')
        .and_then(|i| reply[i..].find('}').map(|j| &reply[i..=i + j]))
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let Some(json) = json else { return Ok(Judgement::AskOwner) };
    if json.get("settled").and_then(|v| v.as_bool()) != Some(true) {
        return Ok(Judgement::AskOwner);
    }
    let picked = json.get("answer").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    // the answer must be one of the card's options, copied exactly (or its text without the letter)
    let known = card.options.lines().any(|line| {
        let line = line.trim();
        line == picked || line.split_once(':').map(|(_, text)| text.trim() == picked).unwrap_or(false)
    });
    if picked.is_empty() || !known {
        return Ok(Judgement::AskOwner);
    }
    let why = json.get("why").and_then(|v| v.as_str()).unwrap_or("a precedent covers it");
    answer(ctx, id, &picked, "system (precedent)", why)?;
    Ok(Judgement::Precedent(picked))
}
