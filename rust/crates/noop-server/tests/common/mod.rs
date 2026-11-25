//! Shared test harness: an in-memory server with a controllable clock.
#![allow(dead_code)]
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use base64::Engine;
use http_body_util::BodyExt;
use noop_server::app::{App, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

pub const TOKEN: &str = "machine-token";
pub const OWNER: &str = "owner@example.com";

pub struct T {
    pub app: Arc<App>,
    pub clock: Arc<AtomicI64>,
}

pub fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}
pub fn unb64(s: &Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s.as_str().unwrap()).unwrap()
}

impl T {
    pub fn new() -> T {
        Self::with(Config { hub_token: TOKEN.into(), owners: vec![OWNER.into()], en: true, ..Default::default() })
    }
    pub fn with_web(dir: std::path::PathBuf) -> T {
        Self::with(Config { hub_token: TOKEN.into(), owners: vec![OWNER.into()], en: true, web_dir: Some(dir), ..Default::default() })
    }
    pub fn with(cfg: Config) -> T {
        let clock = Arc::new(AtomicI64::new(1_780_000_000_000));
        let c2 = clock.clone();
        let app = App::new(rusqlite::Connection::open_in_memory().unwrap(), cfg, Some(Arc::new(move || c2.load(Ordering::SeqCst)))).unwrap();
        T { app, clock }
    }
    /// POST a multipart form with one `file` field.
    pub async fn upload(&self, path: &str, auth: &str, filename: Option<&str>, ty: Option<&str>, data: &[u8]) -> (u16, Value) {
        let mut body = Vec::new();
        body.extend_from_slice(b"--XBOUNDARY\r\nContent-Disposition: form-data; name=\"file\"");
        if let Some(f) = filename {
            body.extend_from_slice(format!("; filename=\"{f}\"").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        if let Some(t) = ty {
            body.extend_from_slice(format!("Content-Type: {t}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n--XBOUNDARY--\r\n");
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", format!("Bearer {auth}"))
            .header("content-type", "multipart/form-data; boundary=XBOUNDARY")
            .body(Body::from(body))
            .unwrap();
        let res = noop_server::router(self.app.clone()).oneshot(req).await.unwrap();
        let status = res.status().as_u16();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
    /// Any request; the body as bytes and the headers.
    pub async fn bytes(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> (u16, Vec<u8>, std::collections::HashMap<String, String>) {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let res = noop_server::router(self.app.clone()).oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status().as_u16();
        let h = res.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string())).collect();
        (status, res.into_body().collect().await.unwrap().to_bytes().to_vec(), h)
    }
    /// A request without auth; the body as text and the headers.
    pub async fn raw(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> (u16, String, std::collections::HashMap<String, String>) {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let res = noop_server::router(self.app.clone()).oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status().as_u16();
        let h = res.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string())).collect();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned(), h)
    }
    pub async fn call(&self, method: &str, path: &str, auth: Option<&str>, body: Option<Value>) -> (u16, Value) {
        let mut b = Request::builder().method(method).uri(path).header("x-noop-host", "run");
        if let Some(a) = auth {
            b = b.header("authorization", format!("Bearer {a}"));
        }
        let req = b.body(body.map(|v| Body::from(v.to_string())).unwrap_or_else(Body::empty)).unwrap();
        let res = noop_server::router(self.app.clone()).oneshot(req).await.unwrap();
        let status = res.status().as_u16();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
    pub async fn machine(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        self.call(method, path, Some(TOKEN), body).await
    }
    pub fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }
    pub fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }
    /// Bind 2FA from a machine, sign in, and return (owner session, TOTP secret).
    pub async fn sign_in(&self) -> (String, String) {
        let (s, r) = self.machine("POST", "/api/host/2fa/setup", None).await;
        assert_eq!(s, 200);
        let secret = r["secret"].as_str().unwrap().to_string();
        assert!(r["uri"].as_str().unwrap().starts_with("otpauth://totp/Noop%3Aowner%40example.com?secret="));
        let (s, r) = self.machine("POST", "/api/host/2fa/confirm", Some(json!({ "totp": self.totp(&secret) }))).await;
        assert_eq!((s, &r["replaced"]), (200, &json!(false)), "{r}");
        self.advance(30_000); // the confirm used this step's code
        let (s, r) = self.call("POST", "/api/auth/start", None, Some(json!({ "email": OWNER }))).await;
        assert_eq!((s, &r["delivery"]), (200, &json!("cli")), "no SMTP: the code goes to `noop login`: {r}");
        let (s, r) = self.machine("GET", "/api/host/login-code", None).await;
        assert_eq!(s, 200);
        let code = r["code"].as_str().unwrap().to_string();
        let (s, r) = self
            .call("POST", "/api/auth/verify", None, Some(json!({ "email": OWNER, "code": code.to_lowercase(), "totp": self.totp(&secret) })))
            .await;
        assert_eq!(s, 200, "{r}");
        (r["session"].as_str().unwrap().to_string(), secret)
    }
    pub fn totp(&self, secret: &str) -> String {
        let key = noop_core::totp::b32decode(secret).unwrap();
        noop_core::totp::hotp(&key, noop_core::totp::step_at(self.now()) as u64)
    }
}

