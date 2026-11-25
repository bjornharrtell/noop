//! Read-only views of the shared data files: tasks, decisions, channels. Port of the data-view parts of
//! cloud/src/tools.ts. The tool list and write ops (used by MCP and the chat brain) come with the MCP port.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Map, Value};

/// One data file as text.
#[derive(Clone, Debug)]
pub struct FileRow {
    pub path: String,
    pub text: String,
    pub updated: String,
}

/// Where the views read from: the server's files table, or a machine's local mirror.
pub trait Store {
    /// One data file (path relative to the data root), or None.
    fn read(&self, path: &str) -> Option<FileRow>;
    /// Files under `dir` ("tasks/active/"), newest path first.
    fn list(&self, dir: &str) -> Vec<FileRow>;
}

pub const TASK_STATES: [&str; 6] = ["inbox", "active", "waiting", "blocked", "failed", "done"];
/// Cancelled tasks live in rejected/, next to routine copies nobody approved.
pub const HISTORY_STATES: [&str; 7] = ["inbox", "active", "waiting", "blocked", "failed", "done", "rejected"];
pub const STALE_DAYS: i64 = 7;

pub static TASK_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^T-[A-Za-z0-9_-]*\d{4}$").unwrap());
/// What /api/tasks/cancel accepts (no dashes after "T-", as in the Worker).
pub static CANCEL_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^T-[A-Za-z0-9_]*[0-9]{4}$").unwrap());
pub static CHANNEL_SLUG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9-]{0,47}$").unwrap());
static TASK_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^tasks/[^/]+/T-[^/]*\.md$").unwrap());
static CHANNEL_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^channels/[^/]+\.md$").unwrap());
static CANCELLED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^cancelled:").unwrap());
static NOTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^>?\s*(Requeued|Retried|Resumed|Attempt \d+ failed)\b").unwrap());
static RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"## Run (\S+) · (\S+) · (\S+)\n\*\*(\w+)\*\* — ([^\n]*)").unwrap());
static PARAS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\n+").unwrap());
static QUOTE_MARK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^> ?").unwrap());
static REPORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\breports/[\w./-]+\.md\b").unwrap());

/// `---\nk: v\n---\nbody` → (meta, body).
pub fn parse_doc(text: &str) -> (HashMap<String, String>, String) {
    let mut meta = HashMap::new();
    let Some(rest) = text.strip_prefix("---\n") else { return (meta, text.to_string()) };
    // the closing fence is the first "\n---" (lazy match), followed by an optional newline
    let Some(end) = rest.find("\n---") else { return (meta, text.to_string()) };
    let head = &rest[..end];
    let body = &rest[end + 4..];
    let body = body.strip_prefix('\n').unwrap_or(body);
    for line in head.split('\n') {
        if let Some(i) = line.find(':').filter(|&i| i > 0) {
            meta.insert(line[..i].trim().to_string(), line[i + 1..].trim().to_string());
        }
    }
    (meta, body.to_string())
}

fn file_id(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).trim_end_matches(".md").to_string()
}

fn utf16_prefix(s: &str, n: usize) -> String {
    let mut count = 0;
    s.chars()
        .take_while(|c| {
            count += c.len_utf16();
            count <= n
        })
        .collect()
}

/// Insert `k` only when `v` is Some (JSON `undefined` is dropped by JSON.stringify).
fn put(m: &mut Map<String, Value>, k: &str, v: Option<&String>) {
    if let Some(v) = v {
        m.insert(k.into(), Value::from(v.clone()));
    }
}

/// A task as the app shows it.
pub fn history_row(f: &FileRow, folder: &str) -> Map<String, Value> {
    let (m, _) = parse_doc(&f.text);
    let truthy = |k: &str| m.get(k).is_some_and(|v| !v.is_empty());
    let state = if folder == "waiting" && truthy("wake_at") {
        "parked"
    } else if folder == "inbox" && truthy("parked") && truthy("requires") {
        "nohost"
    } else if folder == "rejected" {
        "cancelled"
    } else {
        folder
    };
    let id = m.get("id").filter(|v| !v.is_empty()).cloned().unwrap_or_else(|| file_id(&f.path));
    let finished = m.get("finished").or(m.get("cancelled"));
    let updated = finished.or(m.get("started")).or(m.get("created")).cloned().unwrap_or_else(|| f.updated.clone());
    let mut o = Map::new();
    o.insert("id".into(), id.into());
    o.insert("title".into(), m.get("title").cloned().unwrap_or_default().into());
    put(&mut o, "agent", m.get("agent"));
    put(&mut o, "host", m.get("claimed_by"));
    o.insert("state".into(), state.into());
    if state == "nohost" {
        put(&mut o, "needs", m.get("requires"));
    }
    put(&mut o, "started", m.get("started"));
    put(&mut o, "created", m.get("created"));
    put(&mut o, "finished", finished);
    o.insert("updated".into(), updated.into());
    put(&mut o, "attempt", m.get("attempt"));
    put(&mut o, "cli", m.get("cli_used"));
    put(&mut o, "model", m.get("model"));
    o
}

fn updated_of(r: &Map<String, Value>) -> String {
    r.get("updated").and_then(Value::as_str).unwrap_or("").to_string()
}

/// Every task the fleet has, newest first, a page at a time.
pub fn task_history(st: &dyn Store, query: &str, offset: f64, limit: f64) -> Value {
    let q = query.trim().to_lowercase();
    let mut seen = HashSet::new();
    let mut all: Vec<Map<String, Value>> = Vec::new();
    for s in HISTORY_STATES {
        for f in st.list(&format!("tasks/{s}/")) {
            if !TASK_PATH.is_match(&f.path) || (s == "rejected" && !CANCELLED.is_match(&f.text)) {
                continue;
            }
            let r = history_row(&f, s);
            let id = r["id"].as_str().unwrap_or("").to_string();
            if !seen.insert(id.clone()) {
                continue; // a task copied in two folders mid-sync shows once
            }
            let hay = format!("{id} {} {}", r["title"].as_str().unwrap_or(""), r.get("agent").and_then(Value::as_str).unwrap_or("")).to_lowercase();
            if q.is_empty() || hay.contains(&q) {
                all.push(r);
            }
        }
    }
    all.sort_by_key(|r| std::cmp::Reverse(updated_of(r)));
    let offset = if offset.is_nan() { 0 } else { offset.max(0.0) as usize };
    let limit = if limit.is_nan() || limit == 0.0 { 50.0 } else { limit }.clamp(1.0, 200.0) as usize;
    let total = all.len();
    let tasks: Vec<Value> = all.into_iter().skip(offset).take(limit).map(Value::Object).collect();
    json!({ "tasks": tasks, "total": total })
}

/// The owner's request (a quote or the task's first paragraphs) and the last runs, from a task body.
pub fn parse_task_body(title: Option<&str>, body: &str) -> (String, Vec<Value>) {
    let head = body.split("\n## ").next().unwrap_or("");
    let own: Vec<&str> = PARAS.split(head).filter(|p| !NOTE.is_match(p.trim())).collect();
    let joined = own.join("\n\n");
    let source = match find_quote(&joined) {
        Some(q) => q,
        None if !joined.is_empty() => joined.clone(),
        None => title.unwrap_or("").to_string(),
    };
    let request = utf16_prefix(QUOTE_MARK.replace_all(&source, "").trim(), 1200);
    let runs: Vec<Value> = RUN
        .captures_iter(body)
        .map(|r| json!({ "ts": &r[1], "host": &r[2], "cli": &r[3], "status": &r[4], "summary": &r[5] }))
        .collect();
    let skip = runs.len().saturating_sub(5);
    (request, runs.into_iter().skip(skip).collect())
}

/// JS `/(^|\n)> [\s\S]*?(?=\n\n[^>]|$)/` (the regex crate has no lookahead).
fn find_quote(s: &str) -> Option<String> {
    let start = if s.starts_with("> ") { 0 } else { s.find("\n> ")? };
    let from = start + if start == 0 { 2 } else { 3 };
    let b = s.as_bytes();
    let mut e = from;
    while e < b.len() {
        if b[e] == b'\n' && b.get(e + 1) == Some(&b'\n') && b.get(e + 2).is_some_and(|&c| c != b'>') && s.is_char_boundary(e) {
            break;
        }
        e += 1;
    }
    Some(s[start..e].to_string())
}

/// Every open and closed decision card's metadata.
fn decision_metas(st: &dyn Store) -> Vec<(HashMap<String, String>, FileRow, &'static str)> {
    ["open", "closed"]
        .into_iter()
        .flat_map(|w| st.list(&format!("decisions/{w}/")).into_iter().map(move |d| (parse_doc(&d.text).0, d, w)))
        .collect()
}

/// One task for the app's task page, wherever it is.
pub fn task_info(st: &dyn Store, id: &str) -> Option<Map<String, Value>> {
    for s in HISTORY_STATES {
        let Some(f) = st.read(&format!("tasks/{s}/{id}.md")) else { continue };
        // the cards it asked: they belong to its conversation
        let decisions: Vec<String> = decision_metas(st)
            .into_iter()
            .filter(|(m, _, _)| m.get("task").map(String::as_str) == Some(id) && m.get("id").is_some_and(|v| !v.is_empty()))
            .map(|(m, _, _)| m["id"].clone())
            .collect();
        let (meta, body) = parse_doc(&f.text);
        let (request, runs) = parse_task_body(meta.get("title").map(String::as_str), &body);
        let mut o = history_row(&f, s);
        o.insert("request".into(), request.into());
        o.insert("runs".into(), runs.into());
        o.insert("decisions".into(), decisions.into());
        return Some(o);
    }
    None
}

#[derive(Clone, Debug)]
pub struct ChannelRow {
    pub slug: String,
    pub name: String,
    pub goal: String,
    pub status: &'static str,
    pub created: String,
}

pub fn parse_channel(path: &str, text: &str) -> Option<ChannelRow> {
    let slug = file_id(path);
    if !CHANNEL_SLUG.is_match(&slug) {
        return None;
    }
    let (meta, body) = parse_doc(text);
    Some(ChannelRow {
        name: meta.get("name").filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| slug.clone()),
        goal: meta.get("goal").cloned().unwrap_or(body).trim().to_string(),
        status: if meta.get("status").map(String::as_str) == Some("archived") { "archived" } else { "active" },
        created: meta.get("created").cloned().unwrap_or_default(),
        slug,
    })
}

pub fn list_channel_rows(st: &dyn Store) -> Vec<ChannelRow> {
    let mut rows: Vec<ChannelRow> = st
        .list("channels/")
        .into_iter()
        .filter(|f| CHANNEL_PATH.is_match(&f.path))
        .filter_map(|f| parse_channel(&f.path, &f.text))
        .collect();
    rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.name.cmp(&b.name)));
    rows
}

fn task_channels(meta: &HashMap<String, String>) -> Vec<String> {
    meta.get("channels").map(|c| c.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default()
}

/// JS `Date.parse` for the timestamps the data files use, in ms.
fn parse_ms(s: &str) -> Option<i64> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t.timestamp_millis());
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis())
}

/// Every channel with its items (tasks, the cards they asked, their reports), newest first; `stale` = no activity for
/// STALE_DAYS.
pub fn channels_view(st: &dyn Store, now_ms: i64, limit: usize) -> Value {
    let chans = list_channel_rows(st);
    let mut items: BTreeMap<String, Vec<Map<String, Value>>> = chans.iter().map(|c| (c.slug.clone(), Vec::new())).collect();
    let mut task_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen = HashSet::new();
    let mut report_exists: HashMap<String, bool> = HashMap::new();
    let item = |id: &str, kind: &str, title: &str, state: &str, updated: &str, task: Option<&str>| {
        let mut m = Map::new();
        m.insert("id".into(), id.into());
        m.insert("kind".into(), kind.into());
        m.insert("title".into(), title.into());
        m.insert("state".into(), state.into());
        m.insert("updated".into(), updated.into());
        if let Some(t) = task {
            m.insert("task".into(), t.into());
        }
        m
    };
    for s in HISTORY_STATES {
        for f in st.list(&format!("tasks/{s}/")) {
            if !TASK_PATH.is_match(&f.path) {
                continue;
            }
            let (meta, body) = parse_doc(&f.text);
            let cs: Vec<String> = task_channels(&meta).into_iter().filter(|c| items.contains_key(c)).collect();
            let id = meta.get("id").filter(|v| !v.is_empty()).cloned().unwrap_or_else(|| file_id(&f.path));
            if cs.is_empty() || seen.contains(&id) {
                continue;
            }
            seen.insert(id.clone());
            task_of.insert(id.clone(), cs.clone());
            let row = history_row(&f, s);
            let title = row["title"].as_str().unwrap_or("").to_string();
            let updated = updated_of(&row);
            let mut reports: Vec<String> = Vec::new();
            for m in REPORT.find_iter(&body) {
                if !reports.contains(&m.as_str().to_string()) {
                    reports.push(m.as_str().to_string());
                }
            }
            for c in &cs {
                let list = items.get_mut(c).expect("filtered above");
                list.push(item(&id, "task", &title, s, &updated, None));
                for path in &reports {
                    if path.split('/').any(|s| s == "..") {
                        continue;
                    }
                    let exists = *report_exists.entry(path.clone()).or_insert_with(|| st.read(path).is_some());
                    if !exists || list.iter().any(|i| i["id"] == path.as_str()) {
                        continue;
                    }
                    let name = file_id(path);
                    list.push(item(path, "report", &name, "done", &updated, Some(&id)));
                }
            }
        }
    }
    for (meta, d, w) in decision_metas(st) {
        let (Some(task), Some(id)) = (meta.get("task"), meta.get("id")) else { continue };
        let Some(cs) = task_of.get(task) else { continue };
        if id.is_empty() {
            continue;
        }
        let title = meta.get("question").unwrap_or(id);
        let updated = meta.get("answered").or(meta.get("created")).cloned().unwrap_or_else(|| d.updated.clone());
        for c in cs {
            items.get_mut(c).expect("task_of holds known channels").push(item(id, "decision", title, w, &updated, Some(task)));
        }
    }
    let channels: Vec<Value> = chans
        .iter()
        .map(|c| {
            let mut list = items.remove(&c.slug).unwrap_or_default();
            list.sort_by_key(|i| std::cmp::Reverse(updated_of(i)));
            let last = list.first().map(updated_of).unwrap_or_else(|| c.created.clone());
            let stale = last.is_empty() || parse_ms(&last).is_some_and(|t| now_ms - t > STALE_DAYS * 86_400_000);
            let refs: Vec<Value> = list.iter().filter(|i| i["kind"] != "report").map(|i| i["id"].clone()).collect();
            let count = list.len();
            list.truncate(limit);
            json!({ "slug": c.slug, "name": c.name, "goal": c.goal, "status": c.status, "created": c.created,
                "last_activity": last, "stale": stale, "count": count, "refs": refs, "items": list })
        })
        .collect();
    json!({ "channels": channels })
}

/// A channel's digest: the main-chat messages about its tasks and cards, newest last. Task-thread chatter and the
/// hub's own acks stay on the task page.
pub fn channel_digest(events: Vec<Value>, refs: &[String], slug: &str, limit: usize) -> Vec<Value> {
    let want: HashSet<&str> = refs.iter().map(String::as_str).collect();
    let out: Vec<Value> = events
        .into_iter()
        .filter(|e| {
            if e.get("thread").is_some_and(|t| t.as_bool().unwrap_or(!t.is_null())) {
                return false;
            }
            match e.get("channel").and_then(Value::as_str).filter(|c| !c.is_empty()) {
                Some(c) => c == slug,
                None => {
                    let r = e.get("ref").and_then(Value::as_str).unwrap_or("");
                    !r.is_empty()
                        && want.contains(r)
                        && !e.get("from").and_then(Value::as_str).unwrap_or("").starts_with("hub")
                        && e.get("kind").and_then(Value::as_str) != Some("system")
                }
            }
        })
        .collect();
    let skip = out.len().saturating_sub(limit);
    out.into_iter().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(Vec<FileRow>);
    impl Store for Mem {
        fn read(&self, p: &str) -> Option<FileRow> {
            self.0.iter().find(|f| f.path == p).cloned()
        }
        fn list(&self, dir: &str) -> Vec<FileRow> {
            let mut v: Vec<FileRow> = self.0.iter().filter(|f| f.path.starts_with(dir)).cloned().collect();
            v.sort_by(|a, b| b.path.cmp(&a.path));
            v
        }
    }
    fn f(path: &str, text: &str) -> FileRow {
        FileRow { path: path.into(), text: text.into(), updated: "2026-11-20T00:00:00.000Z".into() }
    }

    #[test]
    fn doc() {
        let (m, b) = parse_doc("---\nid: T-0001\ntitle: a: b\n---\nbody\n");
        assert_eq!((m["id"].as_str(), m["title"].as_str(), b.as_str()), ("T-0001", "a: b", "body\n"));
        assert_eq!(parse_doc("plain").1, "plain");
    }

    #[test]
    fn task_body() {
        let body = "> Requeued after a crash\n\n> Please ship the\n> release\n\nmore text\n\n## Run 2026-11-20 · h1 · claude\n**ok** — shipped";
        let (req, runs) = parse_task_body(Some("t"), body);
        assert_eq!(req, "Please ship the\nrelease");
        assert_eq!(runs[0]["host"], "h1");
        assert_eq!(parse_task_body(Some("title"), "").0, "title");
    }

    #[test]
    fn history_and_channels() {
        let st = Mem(vec![
            f("tasks/active/T-0001.md", "---\nid: T-0001\ntitle: Ship\nchannels: rel\nstarted: 2026-11-21T00:00:00.000Z\n---\nsee reports/r1.md\n"),
            f("tasks/rejected/T-0002.md", "---\nid: T-0002\ntitle: Old\ncancelled: 2026-11-19T00:00:00.000Z\n---\n"),
            f("tasks/rejected/T-0003.md", "---\nid: T-0003\ntitle: Never approved\n---\n"),
            f("reports/r1.md", "report"),
            f("decisions/open/D-0001.md", "---\nid: D-0001\nquestion: Which?\ntask: T-0001\ncreated: 2026-11-22T00:00:00.000Z\n---\n"),
            f("channels/rel.md", "---\nname: Release\n---\nShip it\n"),
        ]);
        let h = task_history(&st, "", f64::NAN, f64::NAN);
        assert_eq!(h["total"], 2, "unapproved routine copies are not history: {h}");
        assert_eq!(h["tasks"][1]["state"], "cancelled");
        let info = task_info(&st, "T-0001").unwrap();
        assert_eq!(info["decisions"], json!(["D-0001"]));
        let ms = parse_ms("2026-11-23T00:00:00.000Z").unwrap();
        let v = channels_view(&st, ms, 100);
        let c = &v["channels"][0];
        assert_eq!((c["name"].as_str(), c["goal"].as_str(), c["count"].as_i64()), (Some("Release"), Some("Ship it"), Some(3)));
        assert_eq!(c["refs"], json!(["D-0001", "T-0001"]));
        assert_eq!(c["stale"], false);
        let evs = vec![
            json!({ "from": "owner", "ref": "T-0001", "kind": "owner" }),
            json!({ "from": "hub", "ref": "T-0001", "kind": "notice" }),
            json!({ "from": "a", "ref": "T-0001", "thread": true }),
            json!({ "from": "owner", "channel": "rel" }),
            json!({ "from": "owner", "channel": "other" }),
        ];
        assert_eq!(channel_digest(evs, &["T-0001".into()], "rel", 80).len(), 2);
    }
}
