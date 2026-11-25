//! Live sockets over a real listening server.
mod common;

use std::time::Duration;

use common::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn listen(t: &T) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(noop_server::serve_on(t.app.clone(), l));
    format!("ws://{addr}")
}

async fn connect(url: String) -> Ws {
    tokio_tungstenite::connect_async(url).await.expect("websocket connects").0
}

/// The next text frame as JSON (skipping frames `skip` accepts), within 2 s.
async fn next(ws: &mut Ws, skip: impl Fn(&Value) -> bool) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(2), ws.next()).await.expect("a frame in time").unwrap().unwrap();
        if let Message::Text(t) = m {
            let v: Value = serde_json::from_str(&t).unwrap_or(Value::String(t.to_string()));
            if !skip(&v) {
                return v;
            }
        }
    }
}

#[tokio::test]
async fn owner_and_host_sockets() {
    let t = T::new();
    let (session, _) = t.sign_in().await;
    let base = listen(&t).await;

    // the primary connects first: it counts as online without heartbeats
    let mut host = connect(format!("{base}/api/host/stream?token={TOKEN}")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    t.advance(10 * 60_000);
    let (_, st) = t.call("GET", "/api/status", Some(&session), None).await;
    assert_eq!(st["hub"]["online"], true);

    // an owner opens the app: the primary hears there is a watcher
    let mut owner = connect(format!("{base}/api/stream?v=2&token={session}")).await;
    // (the primary's own connect told it n = 0 first, as the Worker does)
    assert_eq!(next(&mut host, |v| v["n"] == 0).await, json!({ "type": "watchers", "n": 1 }));

    // a fleet event reaches the owner live
    t.machine("POST", "/api/events", Some(json!({ "from": "dev", "kind": "result", "text": "done" }))).await;
    let ev = next(&mut owner, |v| v.get("$status").is_some()).await;
    assert_eq!(ev["text"], "done");

    // the owner's message nudges the primary instead of waiting for a poll
    t.call("POST", "/api/messages", Some(&session), Some(json!({ "text": "hi" }))).await;
    assert_eq!(next(&mut host, |_| false).await, json!({ "type": "inbox" }));
    let echoed = next(&mut owner, |v| v.get("$status").is_some()).await;
    assert_eq!(echoed["text"], "hi");

    // the dashboard is pushed to ?v=2 clients
    t.machine("PUT", "/api/host/state", Some(json!({ "status": { "tasks": 7 } }))).await;
    let st = next(&mut owner, |v| v.get("$status").is_none()).await;
    assert_eq!((st["$status"]["tasks"].as_i64(), st["$status"]["hub"]["online"].as_bool()), (Some(7), Some(true)));

    // keepalive
    owner.send(Message::Text("ping".into())).await.unwrap();
    assert_eq!(next(&mut owner, |v| v.is_object()).await, json!("pong"));

    // the owner leaves: the primary hears it
    owner.close(None).await.unwrap();
    assert_eq!(next(&mut host, |_| false).await, json!({ "type": "watchers", "n": 0 }));
    host.close(None).await.unwrap();
}

#[tokio::test]
async fn host_stream_is_machine_only() {
    let t = T::new();
    let (session, _) = t.sign_in().await;
    let base = listen(&t).await;
    let r = tokio_tungstenite::connect_async(format!("{base}/api/host/stream?token={session}")).await;
    assert!(r.is_err(), "an owner session cannot open the primary's socket");
    let r = tokio_tungstenite::connect_async(format!("{base}/api/stream")).await;
    assert!(r.is_err(), "no credential, no socket");
    let (s, r) = t.call("GET", "/api/stream", Some(&session), None).await;
    assert_eq!((s, r["error"].as_str()), (426, Some("websocket expected")));
}
