//! Server state, configuration and the small helpers every route uses.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use rusqlite::Connection;
use serde_json::Value;

use crate::mail::SmtpCfg;

/// Settings read from the environment (the same names the Worker used, so `noop cloud deploy` values carry over).
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// HUB_TOKEN: the machine (fleet) credential.
    pub hub_token: String,
    /// HUB_OWNER_EMAIL: owner allow-list, comma-separated.
    pub owners: Vec<String>,
    /// HUB_SMTP: JSON SmtpCfg. Without it, login codes are shown by `noop login` on a machine.
    pub smtp: Option<SmtpCfg>,
    /// HUB_LANG: "en…" gives English text, anything else Simplified Chinese.
    pub en: bool,
    /// NOOP_CLIENT_IP_HEADER: header the reverse proxy puts the client address in (e.g. x-forwarded-for).
    pub client_ip_header: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let hub_token = var("HUB_TOKEN").ok_or("HUB_TOKEN is not set")?;
        if var("HUB_ALLOW_IPS").is_some() {
            // fail closed: an allow-list the server ignores would leave the owner believing it is enforced
            return Err("HUB_ALLOW_IPS is not supported by this server yet; unset it or restrict access at the firewall".into());
        }
        let smtp = match var("HUB_SMTP") {
            Some(s) => Some(serde_json::from_str::<SmtpCfg>(&s).map_err(|e| format!("HUB_SMTP is not valid JSON: {e}"))?),
            None => None,
        };
        Ok(Config {
            hub_token,
            owners: var("HUB_OWNER_EMAIL").map(|s| parse_owners(&s)).unwrap_or_default(),
            smtp: smtp.filter(|s| !s.host.is_empty()),
            en: var("HUB_LANG").is_some_and(|l| l.to_lowercase().starts_with("en")),
            client_ip_header: var("NOOP_CLIENT_IP_HEADER").map(|h| h.to_lowercase()),
        })
    }
}

pub fn parse_owners(s: &str) -> Vec<String> {
    s.split(',').map(norm_email).filter(|e| !e.is_empty()).collect()
}

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// One process, one SQLite connection: every request runs its reads and writes under this lock, which gives the same
/// one-writer guarantee the Durable Object gave (task claims rely on it).
pub struct App {
    pub db: Mutex<Connection>,
    pub cfg: Config,
    clock: Clock,
}

impl App {
    pub fn new(conn: Connection, cfg: Config, clock: Option<Clock>) -> rusqlite::Result<Arc<App>> {
        crate::db::init(&conn)?;
        Ok(Arc::new(App { db: Mutex::new(conn), cfg, clock: clock.unwrap_or_else(|| Arc::new(|| chrono::Utc::now().timestamp_millis())) }))
    }
    pub fn now(&self) -> i64 {
        (self.clock)()
    }
    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// The server's text language (HUB_LANG).
    pub fn l(&self, en: &str, zh: &str) -> String {
        if self.cfg.en { en.to_string() } else { zh.to_string() }
    }
    pub fn owners(&self) -> &[String] {
        &self.cfg.owners
    }
    pub fn owner_ok(&self, email: &str) -> bool {
        let o = self.owners();
        o.is_empty() || o.iter().any(|x| *x == norm_email(email))
    }
    /// JS `new Date(ms).toISOString()`.
    pub fn iso(&self, ms: i64) -> String {
        iso(ms)
    }
}

pub fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// A request reduced to what the routes read.
pub struct Req {
    pub method: Method,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HeaderMap,
    pub body: axum::body::Bytes,
    pub peer: Option<SocketAddr>,
}

impl Req {
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers.get(k).and_then(|v| v.to_str().ok())
    }
    /// `URLSearchParams.get`: the first value.
    pub fn q(&self, k: &str) -> Option<&str> {
        self.query.get(k).map(String::as_str)
    }
    /// `await req.json().catch(() => null)`.
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
    /// The client address: the proxy's header when configured, else the socket peer.
    pub fn ip(&self, cfg: &Config) -> String {
        if let Some(h) = &cfg.client_ip_header {
            if let Some(v) = self.header(h) {
                return v.split(',').next().unwrap_or("").trim().to_string();
            }
        }
        self.peer.map(|p| p.ip().to_string()).unwrap_or_default()
    }
}

pub fn parse_query(q: Option<&str>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for part in q.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        out.entry(pct_decode(k)).or_insert_with(|| pct_decode(v));
    }
    out
}

/// application/x-www-form-urlencoded decoding ('+' is a space).
pub fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hi = (b[i + 1] as char).to_digit(16);
                let lo = (b[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn json(v: Value, status: u16) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&v).unwrap_or_default()))
        .expect("valid response")
}

pub fn ok(v: Value) -> Response {
    json(v, 200)
}

pub fn err(msg: impl Into<String>, status: u16) -> Response {
    json(serde_json::json!({ "error": msg.into() }), status)
}

/// JS `Number(s)` for a query parameter: missing → NaN, "" → 0.
pub fn js_num(s: Option<&str>) -> f64 {
    match s {
        None => f64::NAN,
        Some(t) if t.trim().is_empty() => 0.0,
        Some(t) => t.trim().parse::<f64>().unwrap_or(f64::NAN),
    }
}

/// JS `Number(v)` for a JSON value.
pub fn js_num_value(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => *b as i32 as f64,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => js_num(Some(s)),
        Some(_) => f64::NAN,
    }
}

/// `x || fallback` for a number.
pub fn or(n: f64, fallback: f64) -> f64 {
    if n.is_nan() || n == 0.0 { fallback } else { n }
}

/// A JSON number that is an integer when it can be.
pub fn num(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9e15 { Value::from(n as i64) } else { Value::from(n) }
}

pub fn sha256hex(b: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(b))
}

pub fn norm_email(e: &str) -> String {
    e.trim().to_lowercase()
}

/// `String(v).slice(0, n)` on a JSON value, by characters.
pub fn js_string(v: Option<&Value>, n: usize) -> String {
    let s = match v {
        None => "undefined".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Object(_)) => "[object Object]".to_string(),
        Some(other) => other.to_string(),
    };
    s.chars().take(n).collect()
}

pub fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
