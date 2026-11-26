//! Keeping the queue honest: a task claimed by a machine that never finished it goes back to the inbox.
//!
//! Ported from the reaping half of the TypeScript supervisor. The verdict, escalation and decision machinery is not
//! here: it judges runs with a model, and Noop's judgement prompts have to be rewritten for Copilot first.

use std::collections::BTreeMap;

use crate::work::{read_doc, write_doc};
use crate::Ctx;

/// `fleet.reap_after_minutes`, default 20 as the template has it.
pub fn reap_after_minutes(ctx: &Ctx) -> i64 {
    std::fs::read_to_string(ctx.mirror().join("config.toml"))
        .ok()
        .and_then(|t| t.parse::<toml::Value>().ok())
        .and_then(|d| d.get("fleet")?.get("reap_after_minutes")?.as_integer())
        .unwrap_or(20)
}

/// `2026-11-26T09:14:00Z` (what a claim writes) to milliseconds since the epoch.
pub fn parse_stamp(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day) = (d.next()?.parse::<i64>().ok()?, d.next()?.parse::<i64>().ok()?, d.next()?.parse::<i64>().ok()?);
    let mut t = rest.trim_end_matches('Z').split(':');
    let (hh, mm) = (t.next()?.parse::<i64>().ok()?, t.next()?.parse::<i64>().ok()?);
    let y0 = if m <= 2 { y - 1 } else { y };
    let era = if y0 >= 0 { y0 } else { y0 - 399 } / 400;
    let yoe = y0 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + hh) * 60 + mm) * 60_000)
}

/// Move claims older than `reap_after_minutes` back to the inbox. Returns how many were released.
pub fn reap(ctx: &Ctx, host: &str, now_ms: i64) -> Result<usize, String> {
    let limit = reap_after_minutes(ctx).max(1) * 60_000;
    let dir = ctx.mirror().join("tasks/active");
    let Ok(rd) = std::fs::read_dir(&dir) else { return Ok(0) };
    let mut released = 0;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(doc) = read_doc(&path) else { continue };
        let started = doc.meta.get("started").and_then(|s| parse_stamp(s)).or_else(|| {
            let m = std::fs::metadata(&path).ok()?.modified().ok()?;
            let d = m.duration_since(std::time::UNIX_EPOCH).ok()?;
            Some(d.as_millis() as i64)
        });
        let Some(started) = started else { continue };
        if now_ms - started < limit {
            continue;
        }
        let id = doc.meta.get("id").cloned().or_else(|| path.file_stem().map(|s| s.to_string_lossy().to_string())).unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let mut meta: BTreeMap<String, String> = doc.meta.clone();
        meta.remove("claimed_by");
        meta.remove("started");
        meta.insert("reaped".into(), format!("by {host}"));
        write_doc(&ctx.mirror().join(format!("tasks/inbox/{id}.md")), &meta, &doc.body)?;
        std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        released += 1;
    }
    Ok(released)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("noop-sup-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("data/tasks/active")).unwrap();
        std::fs::create_dir_all(d.join("data/tasks/inbox")).unwrap();
        d
    }

    fn claim(ctx: &Ctx, id: &str, started: &str) {
        let mut meta = BTreeMap::new();
        meta.insert("id".into(), id.into());
        meta.insert("claimed_by".into(), "host-a".into());
        meta.insert("started".into(), started.into());
        write_doc(&ctx.mirror().join(format!("tasks/active/{id}.md")), &meta, "work").unwrap();
    }

    #[test]
    fn timestamps_parse() {
        assert_eq!(parse_stamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_stamp("2000-01-01T00:00:00Z"), Some(946_684_800_000), "a known epoch value");
        assert_eq!(parse_stamp("2026-11-26T10:00:00Z"), Some(498_802 * 3_600_000));
        assert_eq!(parse_stamp("nonsense"), None);
    }

    #[test]
    fn a_stale_claim_goes_back_and_a_fresh_one_stays() {
        let h = home("a");
        let ctx = Ctx::new("http://x", "k", &h);
        let now = parse_stamp("2026-11-26T12:00:00Z").unwrap();
        claim(&ctx, "T-0001", "2026-11-26T09:00:00Z");
        claim(&ctx, "T-0002", "2026-11-26T11:55:00Z");

        let released = reap(&ctx, "host-b", now).unwrap();
        assert_eq!(released, 1);
        assert!(ctx.mirror().join("tasks/inbox/T-0001.md").is_file());
        assert!(!ctx.mirror().join("tasks/active/T-0001.md").exists());
        assert!(ctx.mirror().join("tasks/active/T-0002.md").is_file(), "the fresh claim stays");
        let doc = read_doc(&ctx.mirror().join("tasks/inbox/T-0001.md")).unwrap();
        assert!(!doc.meta.contains_key("claimed_by"), "the claim is dropped: {:?}", doc.meta);

        std::fs::remove_dir_all(h).ok();
    }
}
