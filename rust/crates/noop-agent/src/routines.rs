//! Scheduled routines: a `[[routine]]` in `config.toml` becomes a task when its time comes.
//!
//! Ported from the TypeScript client's `due` and the routine half of `runRoutines`. The schedule is a small grammar:
//! `15m`, `hourly`, `daily HH:MM`, `weekdays HH:MM`, `weekly MON HH:MM`. Times are UTC here; a routine fires at the most
//! recent slot at or before now, and a machine that was asleep catches up instead of skipping the day.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::work::{read_doc, write_doc};
use crate::Ctx;

/// A routine from `config.toml`.
#[derive(Debug, Clone, Default)]
pub struct Routine {
    pub name: String,
    pub every: String,
    pub scheduler: String,
    pub agent: String,
    pub priority: String,
    pub title: String,
    pub prompt: String,
    pub enabled: bool,
    /// `task` (default) or `usage-report`, which writes a report from the files and spends no model request.
    pub kind: String,
}

/// Parse `[[routine]]` entries out of `config.toml`.
pub fn read_routines(ctx: &Ctx) -> Vec<Routine> {
    let Ok(text) = std::fs::read_to_string(ctx.mirror().join("config.toml")) else { return Vec::new() };
    let Ok(doc) = text.parse::<toml::Value>() else { return Vec::new() };
    let Some(list) = doc.get("routine").and_then(|r| r.as_array()) else { return Vec::new() };
    let s = |t: &toml::Value, k: &str| t.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    list.iter()
        .filter(|t| t.is_table())
        .map(|t| Routine {
            name: s(t, "name"),
            every: s(t, "every"),
            scheduler: {
                let v = s(t, "scheduler");
                if v.is_empty() { "primary".into() } else { v }
            },
            agent: s(t, "agent"),
            priority: {
                let v = s(t, "priority");
                if v.is_empty() { "normal".into() } else { v }
            },
            title: {
                let v = s(t, "title");
                if v.is_empty() { s(t, "name") } else { v }
            },
            prompt: {
                let p = s(t, "prompt");
                if p.is_empty() { s(t, "desc") } else { p }
            },
            enabled: t.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
            kind: {
                let v = s(t, "kind");
                if v.is_empty() { "task".into() } else { v }
            },
        })
        .collect()
}

/// Whether a routine is due, given when it last ran. Ported from `due` in src/cli.ts.
pub fn due(every: &str, last_ms: Option<i64>, now_ms: i64) -> bool {
    let parts: Vec<&str> = every.split_whitespace().collect();
    let Some(&first) = parts.first() else { return false };
    if first == "hourly" || (first.len() > 1 && first.ends_with('m') && first[..first.len() - 1].chars().all(|c| c.is_ascii_digit())) {
        let mins: i64 = if first == "hourly" { 60 } else { first[..first.len() - 1].parse().unwrap_or(60) };
        return last_ms.is_none_or(|l| now_ms - l >= mins * 60_000);
    }
    let Some(hm) = parts.last() else { return false };
    let (h, m) = match hm.split_once(':') {
        Some((h, m)) => (h.parse::<i64>().unwrap_or(0), m.parse::<i64>().unwrap_or(0)),
        None => return false,
    };
    const DAY: i64 = 24 * 60 * 60_000;
    // the most recent slot at or before now, stepping back over days the schedule excludes
    let mut target = (now_ms / DAY) * DAY + (h * 60 + m) * 60_000;
    if now_ms < target {
        target -= DAY;
    }
    let weekday = |ms: i64| ((ms / DAY) + 4) % 7; // 1970-01-01 was a Thursday
    let on_schedule = |ms: i64| match first {
        "weekdays" => weekday(ms) != 0 && weekday(ms) != 6,
        "weekly" => {
            let want = parts.get(1).map(|d| d.to_uppercase()).unwrap_or_default();
            const NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
            NAMES[weekday(ms) as usize].starts_with(&want[..want.len().min(3)])
        }
        _ => true,
    };
    for _ in 0..7 {
        if on_schedule(target) {
            break;
        }
        target -= DAY;
    }
    match last_ms {
        // first run ever: only fire near the scheduled time
        None => now_ms - target < 60 * 60_000,
        Some(l) => l < target,
    }
}

fn state_path(ctx: &Ctx) -> PathBuf {
    ctx.home.join("routines.json")
}

fn load_state(ctx: &Ctx) -> BTreeMap<String, i64> {
    std::fs::read_to_string(state_path(ctx))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_state(ctx: &Ctx, st: &BTreeMap<String, i64>) -> Result<(), String> {
    std::fs::create_dir_all(&ctx.home).ok();
    let text = serde_json::to_string(st).map_err(|e| e.to_string())?;
    std::fs::write(state_path(ctx), text).map_err(|e| format!("routines.json: {e}"))
}

/// A copy already queued or running for this routine holds the next run back, as in the TypeScript client.
fn already_queued(ctx: &Ctx, name: &str) -> bool {
    let source = format!("routine:{name}");
    for dir in ["inbox", "active"] {
        let Ok(rd) = std::fs::read_dir(ctx.mirror().join("tasks").join(dir)) else { continue };
        for e in rd.flatten() {
            if let Ok(doc) = read_doc(&e.path()) {
                if doc.meta.get("source").map(String::as_str) == Some(source.as_str()) {
                    return true;
                }
            }
        }
    }
    false
}

/// The next free task id, from the shared helper so routines and the owner's messages never mint the same one.
fn next_id(ctx: &Ctx) -> String {
    crate::work::next_task_id(ctx)
}

/// Create the tasks for every routine that is due on this machine. Returns how many were queued.
///
/// Who runs it: `scheduler = "each"` on every machine (each has its own copy, pinned to it), a host name on that
/// machine, and `primary` on `[fleet] primary` when that is set. Whatever the setting, the run itself is claimed with a
/// compare-and-swap commit, so two machines ticking at the same second cannot queue the same routine twice.
pub async fn queue(ctx: &Ctx, host: &str, now_ms: i64) -> Result<usize, String> {
    let routines = read_routines(ctx);
    if routines.is_empty() {
        return Ok(0);
    }
    let primary = primary_host(ctx);
    let mut st = load_state(ctx);
    let mut queued = 0;
    for r in routines {
        if !r.enabled || r.name.is_empty() {
            continue;
        }
        let mine = match r.scheduler.as_str() {
            "each" => true,
            "primary" => primary.as_deref().map(|p| p == host).unwrap_or(true),
            other => other == host,
        };
        if !mine || !due(&r.every, st.get(&r.name).copied(), now_ms) {
            continue;
        }
        if already_queued(ctx, &r.name) {
            st.insert(r.name.clone(), now_ms);
            continue;
        }
        // claim this run; a machine that loses the commit leaves the routine to the winner
        let marker = ctx.mirror().join(format!("routines/{}.md", r.name));
        let mut meta = BTreeMap::new();
        meta.insert("ran".to_string(), crate::work::today());
        meta.insert("by".to_string(), host.to_string());
        write_doc(&marker, &meta, &format!("last run by {host}\n"))?;
        if crate::commit_once(ctx, &format!("routine {} ran", r.name)).await?.is_none() {
            crate::pull(ctx).await?;
            st.insert(r.name.clone(), now_ms);
            save_state(ctx, &st)?;
            continue;
        }
        if r.kind == "usage-report" {
            // computed from the files, like the TypeScript client's usage routine: no task, no model request
            write_usage_report(ctx)?;
            crate::commit(ctx, "usage report").await?;
            st.insert(r.name.clone(), now_ms);
            continue;
        }
        let id = next_id(ctx);
        let mut tmeta = BTreeMap::new();
        tmeta.insert("id".to_string(), id.clone());
        tmeta.insert("title".to_string(), format!("{} · {}", r.title, host));
        tmeta.insert("priority".to_string(), r.priority.clone());
        tmeta.insert("source".to_string(), format!("routine:{}", r.name));
        tmeta.insert("created".to_string(), crate::work::today());
        if !r.agent.is_empty() {
            tmeta.insert("agent".to_string(), r.agent.clone());
        }
        write_doc(&ctx.mirror().join(format!("tasks/inbox/{id}.md")), &tmeta, &r.prompt)?;
        crate::commit(ctx, &format!("routine {}", r.name)).await?;
        st.insert(r.name.clone(), now_ms);
        queued += 1;
    }
    save_state(ctx, &st)?;
    Ok(queued)
}

/// `[fleet] primary`, when config.toml names one.
fn primary_host(ctx: &Ctx) -> Option<String> {
    std::fs::read_to_string(ctx.mirror().join("config.toml"))
        .ok()
        .and_then(|t| t.parse::<toml::Value>().ok())
        .and_then(|d| d.get("fleet")?.get("primary")?.as_str().map(str::to_string))
        .filter(|p| !p.is_empty())
}

fn path_is_file(p: &std::path::Path) -> bool {
    p.is_file()
}

/// How many tasks sit in each part of the queue, and how many cards wait: the weekly usage report.
pub fn usage_report(ctx: &Ctx) -> String {
    let count = |dir: &str| -> usize {
        std::fs::read_dir(ctx.mirror().join("tasks").join(dir))
            .map(|rd| rd.flatten().filter(|e| path_is_file(&e.path())).count())
            .unwrap_or(0)
    };
    let cards = std::fs::read_dir(ctx.mirror().join("decisions/open")).map(|rd| rd.flatten().count()).unwrap_or(0);
    let journals = std::fs::read_dir(ctx.mirror().join("journal")).map(|rd| rd.flatten().count()).unwrap_or(0);
    format!(
        "# Usage {}\n\n| | |\n|---|---|\n| tasks waiting | {} |\n| tasks running | {} |\n| tasks done | {} |\n| decisions waiting for you | {} |\n| journal days | {} |\n",
        crate::work::today(),
        count("inbox"),
        count("active"),
        count("done"),
        cards,
        journals
    )
}

fn write_usage_report(ctx: &Ctx) -> Result<(), String> {
    let path = ctx.mirror().join(format!("reports/usage/{}.md", crate::work::today()));
    crate::work::write_doc(&path, &BTreeMap::new(), &usage_report(ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 60 * 60_000;
    /// 2026-11-26 09:30 UTC, a Thursday (2026-11-26T10:00Z is 498 802 hours after the epoch).
    fn now() -> i64 {
        498_802 * 3_600_000 - 30 * 60_000
    }

    #[test]
    fn hourly_and_minutes() {
        assert!(due("hourly", None, 0));
        assert!(due("hourly", Some(now() - 61 * 60_000), now()));
        assert!(!due("hourly", Some(now() - 10 * 60_000), now()));
        assert!(due("15m", Some(now() - 16 * 60_000), now()));
        assert!(!due("15m", Some(now() - 5 * 60_000), now()));
    }

    #[test]
    fn daily_fires_once_a_day() {
        assert!(due("daily 09:00", None, now()), "within an hour of the slot");
        assert!(!due("daily 09:00", None, now() + 2 * 60 * 60_000), "not hours later");
        assert!(!due("daily 09:00", Some(now() - 60_000), now()), "already ran today");
        assert!(due("daily 09:00", Some(now() - DAY), now()), "yesterday's run does not hold today back");
    }

    #[test]
    fn a_machine_that_was_asleep_catches_up() {
        // last ran two days ago; the slot was passed while it slept
        assert!(due("daily 03:00", Some(now() - 2 * DAY), now()));
    }

    #[test]
    fn weekly_and_weekdays() {
        // Thursday now; MON slot is the Monday before
        assert!(!due("weekly MON 09:00", Some(now() - 1000), now()), "already ran after that Monday");
        assert!(due("weekly MON 09:00", Some(now() - 7 * DAY), now()), "a week later");
        assert!(due("weekdays 09:00", Some(now() - DAY), now()));
    }
}
