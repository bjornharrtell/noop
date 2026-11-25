//! Feed, inbox, dashboard, settings, tasks and channels.
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use serde_json::{json, Value};

#[tokio::test]
async fn owner_message_reaches_the_inbox() {
    let t = T::new();
    let (session, _) = t.sign_in().await;
    // no host heard from yet: the owner is told the team is offline
    let (s, ev) = t.call("POST", "/api/messages", Some(&session), Some(json!({ "text": "hello", "client_id": "c1" }))).await;
    assert_eq!(s, 200, "{ev}");
    assert_eq!((ev["from"].as_str(), ev["kind"].as_str()), (Some("owner"), Some("owner")));
    let (_, feed) = t.call("GET", "/api/feed?since=", Some(&session), None).await;
    let kinds: Vec<&str> = feed["events"].as_array().unwrap().iter().filter_map(|e| e["kind"].as_str()).collect();
    assert!(kinds.ends_with(&["owner", "system"]), "{kinds:?}");
    // a retried send with the same client_id is not stored twice
    let (_, again) = t.call("POST", "/api/messages", Some(&session), Some(json!({ "text": "hello", "client_id": "c1" }))).await;
    assert_eq!(again["id"], ev["id"]);

    let (s, r) = t.machine("GET", "/api/host/inbox?after=0", None).await;
    assert_eq!(s, 200);
    let items = r["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!((items[0]["type"].as_str(), items[0]["event"]["text"].as_str()), (Some("message"), Some("hello")));
    let seq = items[0]["seq"].as_i64().unwrap();
    // acknowledged items are gone; the host now counts as online
    let (_, r) = t.machine("GET", &format!("/api/host/inbox?after={seq}"), None).await;
    assert_eq!(r["items"].as_array().unwrap().len(), 0);
    let (_, st) = t.call("GET", "/api/status", Some(&session), None).await;
    assert_eq!(st["hub"]["online"], true);

    // validation
    for (body, want) in [
        (json!({ "text": "  " }), "text required"),
        (json!({ "text": "x", "client_id": "bad id!" }), "bad client_id"),
        (json!({ "text": "x", "attachments": ["/media/../x"] }), "bad attachments"),
        (json!({ "text": "x", "attachments": ["/media/123-a.png"] }), "attachment not found"),
        (json!({ "text": "x", "channel": "nope" }), "channel not found"),
    ] {
        let (s, r) = t.call("POST", "/api/messages", Some(&session), Some(body)).await;
        assert_eq!((s, r["error"].as_str()), (400, Some(want)));
    }
}

#[tokio::test]
async fn inbox_long_poll_wakes_on_a_message() {
    let t = Arc::new(T::new());
    let (session, _) = t.sign_in().await;
    let waiter = {
        let t = t.clone();
        tokio::spawn(async move { t.machine("GET", "/api/host/inbox?after=0&wait=10", None).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = std::time::Instant::now();
    t.call("POST", "/api/fleet/update", Some(&session), None).await;
    let (s, r) = tokio::time::timeout(Duration::from_secs(5), waiter).await.expect("woken, not timed out").unwrap();
    assert_eq!(s, 200);
    assert_eq!(r["items"][0]["type"], "update");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn events_are_idempotent() {
    let t = T::new();
    let ev = json!({ "id": "e-1", "from": "dev", "kind": "result", "text": "done", "ref": "T-0001" });
    let (s, r) = t.machine("POST", "/api/events", Some(ev.clone())).await;
    assert_eq!((s, r["id"].as_str()), (200, Some("e-1")));
    let (_, r) = t.machine("POST", "/api/events", Some(ev)).await;
    assert_eq!(r["duplicate"], true);
}

#[tokio::test]
async fn host_state_and_settings() {
    let t = T::new();
    let (session, _) = t.sign_in().await;
    let (s, r) = t
        .machine("PUT", "/api/host/state", Some(json!({ "status": { "tasks": 3 }, "settings": { "current": { "lang": "en", "nested": { "a": 1 } }, "effective": {}, "options": {} } })))
        .await;
    assert_eq!((s, r["watchers"].as_i64()), (200, Some(0)));
    let (_, st) = t.call("GET", "/api/status", Some(&session), None).await;
    assert_eq!((st["tasks"].as_i64(), st["hub"]["online"].as_bool()), (Some(3), Some(true)));
    // the owner's change is merged into the view and handed to the primary
    let (_, view) = t.call("PUT", "/api/settings", Some(&session), Some(json!({ "nested": { "b": 2 } }))).await;
    assert_eq!(view["current"], json!({ "lang": "en", "nested": { "a": 1, "b": 2 } }));
    let (_, r) = t.machine("GET", "/api/host/inbox?after=0", None).await;
    assert_eq!(r["items"][0], json!({ "seq": r["items"][0]["seq"], "type": "settings", "patch": { "nested": { "b": 2 } } }));
    // a host goes stale after three minutes without a word
    t.advance(4 * 60_000);
    let (_, st) = t.call("GET", "/api/status", Some(&session), None).await;
    assert_eq!(st["hub"]["online"], false);
}

#[tokio::test]
async fn tasks_and_channels() {
    let t = T::new();
    let (session, _) = t.sign_in().await;
    let file = |path: &str, text: &str| json!({ "path": path, "base_version": 0, "content": b64(text.as_bytes()) });
    let (s, _) = t
        .machine("POST", "/api/files/commit", Some(json!({ "changes": [
            file("tasks/active/T-0001.md", "---\nid: T-0001\ntitle: Ship\nchannels: rel\nstarted: 2026-11-21T00:00:00.000Z\n---\n> Ship the release\n"),
            file("tasks/done/T-0002.md", "---\nid: T-0002\ntitle: Old work\nfinished: 2026-11-20T00:00:00.000Z\n---\n"),
            file("decisions/open/D-0001.md", "---\nid: D-0001\nquestion: Which?\ntask: T-0001\n---\n"),
            file("channels/rel.md", "---\nname: Release\ngoal: Ship 1.0\n---\n"),
        ] })))
        .await;
    assert_eq!(s, 200);
    t.machine("POST", "/api/events", Some(json!({ "from": "dev", "kind": "result", "text": "about T-0001", "ref": "T-0001" }))).await;
    t.machine("POST", "/api/events", Some(json!({ "from": "dev", "kind": "decision", "text": "card", "ref": "D-0001" }))).await;
    t.machine("POST", "/api/events", Some(json!({ "from": "dev", "kind": "result", "text": "other", "ref": "T-0002" }))).await;

    let (_, h) = t.call("GET", "/api/tasks?q=ship", Some(&session), None).await;
    assert_eq!((h["total"].as_i64(), h["tasks"][0]["id"].as_str()), (Some(1), Some("T-0001")));
    let (_, info) = t.call("GET", "/api/tasks/get?id=T-0001", Some(&session), None).await;
    assert_eq!(info["request"], "Ship the release");
    let texts: Vec<&str> = info["events"].as_array().unwrap().iter().filter_map(|e| e["text"].as_str()).collect();
    assert_eq!(texts, ["about T-0001", "card"]);
    let (s, _) = t.call("GET", "/api/tasks/get?id=T-9999", Some(&session), None).await;
    assert_eq!(s, 404);

    let (_, v) = t.call("GET", "/api/channels", Some(&session), None).await;
    assert_eq!((v["channels"][0]["slug"].as_str(), v["channels"][0]["count"].as_i64()), (Some("rel"), Some(2)));
    let (_, f) = t.call("GET", "/api/channels/feed?slug=rel", Some(&session), None).await;
    assert_eq!(f["events"].as_array().unwrap().len(), 2);
    let (s, _) = t.call("POST", "/api/messages", Some(&session), Some(json!({ "text": "in channel", "channel": "rel" }))).await;
    assert_eq!(s, 200);

    // owner actions go to the primary through the inbox
    let (s, _) = t.call("POST", "/api/tasks/cancel", Some(&session), Some(json!({ "id": "T-0001", "done": true }))).await;
    assert_eq!(s, 200);
    let (s, _) = t.call("POST", "/api/channels", Some(&session), Some(json!({ "slug": "rel", "archived": true }))).await;
    assert_eq!(s, 200);
    let (s, _) = t.machine("POST", "/api/tasks/cancel", Some(json!({ "id": "T-0001" }))).await;
    assert_eq!(s, 403, "owner-only");
    let (_, r) = t.machine("GET", "/api/host/inbox?after=0", None).await;
    let types: Vec<&str> = r["items"].as_array().unwrap().iter().filter_map(|i| i["type"].as_str()).collect();
    assert_eq!(types, ["message", "cancel", "channel"]);
    assert_eq!(r["items"][2], json!({ "seq": r["items"][2]["seq"], "type": "channel", "slug": "rel", "archived": true }));
    let _: Value = r;
}

#[tokio::test]
async fn web_client_is_served() {
    let dir = std::env::temp_dir().join(format!("noop-web-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), "<html>app</html>").unwrap();
    std::fs::write(dir.join("app.0123456789.js"), "plain").unwrap();
    std::fs::write(dir.join("app.0123456789.js.br"), "brotli").unwrap();
    let t = T::with_web(dir.clone());
    let (s, body, headers) = t.raw("GET", "/", &[]).await;
    assert_eq!((s, body.as_str()), (200, "<html>app</html>"));
    let (_, body, _) = t.raw("GET", "/tasks/T-0001", &[]).await;
    assert_eq!(body, "<html>app</html>", "app routes get index.html");
    let (_, body, h) = t.raw("GET", "/app.0123456789.js", &[("accept-encoding", "gzip, br")]).await;
    assert_eq!((body.as_str(), h.get("content-encoding").map(String::as_str)), ("brotli", Some("br")));
    assert!(h["cache-control"].contains("immutable"));
    let (_, body, h) = t.raw("GET", "/app.0123456789.js", &[]).await;
    assert_eq!((body.as_str(), h.get("content-encoding")), ("plain", None));
    let (s, _, _) = t.raw("GET", "/../Cargo.toml", &[]).await;
    assert_ne!(s, 200);
    let _ = headers;
    std::fs::remove_dir_all(dir).unwrap();
}
