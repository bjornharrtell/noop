//! Web Push against a fake push service on localhost.
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::HeaderMap;
use common::*;
use noop_server::app::Config;
use serde_json::{json, Value};

#[derive(Default)]
struct Seen {
    hits: Vec<(String, HeaderMap, usize)>,
}

/// A push service: /ok answers 201, /gone answers 410.
async fn push_service() -> (String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let s2 = seen.clone();
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri, headers: HeaderMap, body: axum::body::Bytes| {
        let seen = s2.clone();
        async move {
            seen.lock().unwrap().hits.push((uri.path().to_string(), headers, body.len()));
            if uri.path() == "/gone" { axum::http::StatusCode::GONE } else { axum::http::StatusCode::CREATED }
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://127.0.0.1:{}", addr.port()), seen)
}

fn with_push() -> T {
    let (public, private) = noop_server::push::new_vapid_keys();
    T::with(Config {
        hub_token: TOKEN.into(),
        owners: vec![OWNER.into()],
        en: true,
        vapid_public: Some(public),
        vapid_private: Some(private),
        ..Default::default()
    })
}

/// A browser subscription: a real P-256 public key and a 16-byte auth secret.
fn subscription(endpoint: &str) -> Value {
    let (public, _) = noop_server::push::new_vapid_keys();
    json!({ "endpoint": endpoint, "keys": { "p256dh": public, "auth": "BTBZMqHH6r4Tts7J_aSIgg" } })
}

async fn wait_for(seen: &Arc<Mutex<Seen>>, n: usize) {
    for _ in 0..100 {
        if seen.lock().unwrap().hits.len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("push service got {} requests, wanted {n}", seen.lock().unwrap().hits.len());
}

#[tokio::test]
async fn decision_notifies_subscribed_browsers() {
    let t = with_push();
    let (session, _) = t.sign_in().await;
    let (base, seen) = push_service().await;

    let (_, r) = t.call("GET", "/api/push/vapid", Some(&session), None).await;
    assert!(r["publicKey"].as_str().is_some_and(|k| k.len() > 80));
    let (s, _) = t.call("POST", "/api/push/subscribe", Some(&session), Some(json!({ "subscription": subscription(&format!("{base}/ok")), "device_label": "phone" }))).await;
    assert_eq!(s, 200);
    let (s, _) = t.call("POST", "/api/push/subscribe", Some(&session), Some(json!({ "subscription": subscription(&format!("{base}/gone")) }))).await;
    assert_eq!(s, 200);
    let (s, _) = t.call("POST", "/api/push/subscribe", Some(&session), Some(json!({ "subscription": subscription("http://evil.example/x") }))).await;
    assert_eq!(s, 400, "plain http only to localhost");

    // a decision card from the fleet
    t.machine("POST", "/api/events", Some(json!({ "from": "dev", "kind": "decision", "text": "Ship 1.0?", "ref": "D-0001", "options": ["Yes", "No"] }))).await;
    wait_for(&seen, 2).await;
    {
        let s = seen.lock().unwrap();
        let ok = s.hits.iter().find(|h| h.0 == "/ok").expect("the live subscription was notified");
        let h = &ok.1;
        assert!(h["authorization"].to_str().unwrap().starts_with("vapid t="));
        assert_eq!((h["content-encoding"].to_str().unwrap(), h["urgency"].to_str().unwrap(), h["ttl"].to_str().unwrap()), ("aes128gcm", "high", "86400"));
        assert_eq!(h["topic"].to_str().unwrap(), "D-0001");
        assert!(ok.2 > 86, "an encrypted body: header + ciphertext");
    }
    // the gone subscription was removed
    tokio::time::sleep(Duration::from_millis(100)).await;
    let n: i64 = t.app.db().query_row("SELECT COUNT(*) FROM push_subs", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);

    // owner chatter and hub housekeeping do not notify
    t.call("POST", "/api/messages", Some(&session), Some(json!({ "text": "hello" }))).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(seen.lock().unwrap().hits.len(), 2);

    // the test button reaches this session's devices
    let (_, r) = t.call("POST", "/api/push/test", Some(&session), None).await;
    assert_eq!((r["sent"].as_i64(), r["total"].as_i64()), (Some(1), Some(1)));

    // answering from the notification posts the owner's reply
    let (s, ev) = t.call("POST", "/api/push/answer", Some(&session), Some(json!({ "ref": "D-0001", "answer": "A" }))).await;
    assert_eq!((s, ev["text"].as_str(), ev["ref"].as_str()), (200, Some("D-0001 A"), Some("D-0001")));

    let (s, _) = t.machine("POST", "/api/push/subscribe", Some(json!({}))).await;
    assert_eq!(s, 403, "machines cannot subscribe");
}

#[tokio::test]
async fn push_off_without_keys() {
    let t = T::new();
    let (session, _) = t.sign_in().await;
    let (s, _) = t.call("GET", "/api/push/vapid", Some(&session), None).await;
    assert_eq!(s, 503);
}
