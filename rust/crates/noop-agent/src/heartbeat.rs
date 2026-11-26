//! Telling the fleet this machine is alive, so the app can list it.
//!
//! The dashboard is whatever the last machine sent to `PUT /api/host/state`; without that the app shows no machines at
//! all, even while chat replies arrive. Fields are the subset the app reads: the hosts list, the task counts, and the
//! open decision cards. The TypeScript client also sent full task views and channel revisions; those are not built here
//! yet, so the app shows counts and machines rather than a live task list.

use serde_json::json;

use crate::work::read_doc;
use crate::Ctx;

fn count(ctx: &Ctx, kind: &str) -> usize {
    std::fs::read_dir(ctx.mirror().join("tasks").join(kind))
        .map(|rd| rd.flatten().filter(|e| e.path().is_file()).count())
        .unwrap_or(0)
}

/// One minute of load per core, or 0 when it cannot be read.
fn load_ratio() -> f64 {
    let load = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
    (load / cores.max(1.0) * 100.0).round() / 100.0
}

/// The status object the app renders.
pub fn status(ctx: &Ctx, host: &str, busy: usize) -> serde_json::Value {
    let decisions: Vec<serde_json::Value> = crate::decisions::open(ctx)
        .into_iter()
        .map(|c| {
            json!({
                "id": c.id,
                "kind": "decision",
                "question": c.question,
                "held": c.meta.get("held").map(|h| h == "true").unwrap_or(false),
                "task": c.task,
            })
        })
        .collect();
    json!({
        "tasks": {
            "proposed": 0,
            "inbox": count(ctx, "inbox"),
            "active": count(ctx, "active"),
            "waiting": 0,
            "blocked": count(ctx, "blocked"),
            "failed": 0,
            "done": count(ctx, "done"),
        },
        // the task views are not built here yet, so the app shows the counts above and the machines below
        "active": [],
        "recent": [],
        "decisions": decisions,
        "channels_revision": "",
        "hosts": [{
            "host": host,
            "online": true,
            "busy": busy,
            "last_seen": crate::work::today(),
            "code": env!("CARGO_PKG_VERSION"),
            "stats": { "loadRatio": load_ratio() },
        }],
    })
}

/// Send it. Errors are returned so the caller can journal them; the loop keeps going either way.
pub async fn push(ctx: &Ctx, host: &str, busy: usize) -> Result<(), String> {
    let body = json!({ "status": status(ctx, host, busy) });
    let (code, _) = ctx.call("PUT", "/api/host/state", Some(body)).await?;
    if code == 200 {
        Ok(())
    } else {
        Err(format!("host state: server answered {code}"))
    }
}

/// Tasks this machine is running, for the app's busy count.
pub fn busy_count(ctx: &Ctx, host: &str) -> usize {
    let Ok(rd) = std::fs::read_dir(ctx.mirror().join("tasks/active")) else { return 0 };
    rd.flatten()
        .filter(|e| e.path().is_file())
        .filter(|e| read_doc(&e.path()).ok().and_then(|d| d.meta.get("claimed_by").cloned()).as_deref() == Some(host))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_carries_the_host_and_the_counts() {
        let home = std::env::temp_dir().join(format!("noop-beat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("data/tasks/inbox")).unwrap();
        std::fs::create_dir_all(home.join("data/tasks/done")).unwrap();
        let ctx = Ctx::new("http://x", "k", &home);
        std::fs::write(home.join("data/tasks/inbox/T-0001.md"), "---\nid: T-0001\n---\nx").unwrap();
        let s = status(&ctx, "host-a", 0);
        assert_eq!(s["tasks"]["inbox"], 1);
        assert_eq!(s["tasks"]["done"], 0);
        assert_eq!(s["hosts"][0]["host"], "host-a");
        assert_eq!(s["hosts"][0]["online"], true);
        assert!(s["decisions"].as_array().unwrap().is_empty());
        std::fs::remove_dir_all(home).ok();
    }
}
