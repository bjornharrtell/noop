//! Telling the fleet this machine is alive, so the app can list it.
//!
//! The dashboard is whatever the last machine sent to `PUT /api/host/state`; without that the app shows no machines at
//! all, even while chat replies arrive. Fields are the subset the app reads: the hosts list, the task counts and views
//! (running, still open, newest finished), and the open decision cards. The TypeScript client also sent channel
//! revisions and per-task step logs; those are not built here yet, so a task's page shows its title and state but no
//! step history.

use serde_json::json;

use crate::work::read_doc;
use crate::Ctx;

fn count(ctx: &Ctx, kind: &str) -> usize {
    std::fs::read_dir(ctx.mirror().join("tasks").join(kind))
        .map(|rd| rd.flatten().filter(|e| e.path().is_file()).count())
        .unwrap_or(0)
}

/// A task as the app renders it: the fields it reads, not the whole document.
fn view(kind: &str, path: &std::path::Path) -> Option<serde_json::Value> {
    let doc = read_doc(path).ok()?;
    let get = |k: &str| doc.meta.get(k).cloned().unwrap_or_default();
    let id = doc
        .meta
        .get("id")
        .cloned()
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().to_string()))?;
    Some(json!({
        "id": id,
        "title": get("title"),
        "state": kind,
        "agent": get("agent"),
        "priority": if get("priority").is_empty() { "normal".to_string() } else { get("priority") },
        "source": get("source"),
        "host": get("claimed_by"),
        "created": get("created"),
        "failures": get("failures"),
        "blocked_on": get("blocked_on"),
    }))
}

/// Tasks in a folder, newest first by id.
fn views(ctx: &Ctx, kind: &str) -> Vec<serde_json::Value> {
    let Ok(rd) = std::fs::read_dir(ctx.mirror().join("tasks").join(kind)) else { return Vec::new() };
    let mut out: Vec<serde_json::Value> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .filter_map(|p| view(kind, &p))
        .collect();
    out.sort_by(|a, b| b["id"].as_str().unwrap_or("").cmp(a["id"].as_str().unwrap_or("")));
    out
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

/// `[fleet] primary` names the primary machine; with none named, this machine is it (only the primary serves the API).
fn is_primary(ctx: &Ctx, host: &str) -> bool {
    match crate::routines::primary_host(ctx) {
        Some(p) => p == host,
        None => true,
    }
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
    let open = {
        let mut v = views(ctx, "inbox");
        v.extend(views(ctx, "blocked"));
        v.truncate(12);
        v
    };
    let done = {
        let mut v = views(ctx, "done");
        v.truncate(6);
        v
    };
    let active = views(ctx, "active");
    let recent = {
        let mut v = active.clone();
        v.extend(open);
        v.extend(done);
        v
    };
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
        // what the app lists: the tasks being run now, then everything still open and the newest finished
        "active": active,
        "recent": recent,
        "decisions": decisions,
        "channels_revision": "",
        "hosts": [{
            "host": host,
            "online": true,
            "busy": busy,
            // the app marks the primary machine wherever it lists machines
            "primary": is_primary(ctx, host),
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
        for d in ["inbox", "done", "active"] {
            std::fs::create_dir_all(home.join("data/tasks").join(d)).unwrap();
        }
        let ctx = Ctx::new("http://x", "k", &home);
        std::fs::write(home.join("data/tasks/inbox/T-0001.md"), "---\nid: T-0001\ntitle: fix the build\nagent: lead\n---\nx").unwrap();
        std::fs::write(home.join("data/tasks/done/T-0002.md"), "---\nid: T-0002\ntitle: earlier work\n---\ny").unwrap();
        let s = status(&ctx, "host-a", 0);
        assert_eq!(s["tasks"]["inbox"], 1);
        assert_eq!(s["tasks"]["done"], 1);
        assert_eq!(s["hosts"][0]["host"], "host-a");
        assert_eq!(s["hosts"][0]["online"], true);
        assert!(s["decisions"].as_array().unwrap().is_empty());
        // the app lists tasks, not just numbers
        let recent = s["recent"].as_array().unwrap();
        let ids: Vec<&str> = recent.iter().filter_map(|t| t["id"].as_str()).collect();
        assert!(ids.contains(&"T-0001"), "queued task listed: {ids:?}");
        assert!(ids.contains(&"T-0002"), "finished task listed: {ids:?}");
        let first = recent.iter().find(|t| t["id"] == "T-0001").unwrap();
        assert_eq!(first["title"], "fix the build");
        assert_eq!(first["state"], "inbox");
        assert!(s["active"].as_array().unwrap().is_empty());
        std::fs::remove_dir_all(home).ok();
    }
}
