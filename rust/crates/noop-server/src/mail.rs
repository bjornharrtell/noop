//! Email over SMTP (HUB_SMTP), and security alerts. Port of cloud/src/smtp.ts and Hub.alert().
//! Cloudflare Email Routing (the `send_email` binding) has no VPS equivalent; SMTP is the only mail path.
use std::sync::Arc;

use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::json;

use crate::app::{App, Req};

#[derive(Clone, Debug, Deserialize)]
pub struct SmtpCfg {
    pub host: String,
    pub port: Option<u16>,
    pub secure: Option<bool>,
    pub starttls: Option<bool>,
    pub user: Option<String>,
    pub pass: Option<String>,
    pub from: Option<String>,
}

fn is_local(h: &str) -> bool {
    h == "localhost" || h == "127.0.0.1"
}

/// port 465 → implicit TLS · 587 (or secure:false) → STARTTLS · localhost with starttls:false → plain (local tests only)
pub async fn send(c: &SmtpCfg, to: &str, subject: &str, text: &str) -> Result<(), String> {
    let port = c.port.unwrap_or(if c.secure == Some(false) { 587 } else { 465 });
    let user = c.user.clone().filter(|u| !u.is_empty());
    let from = c.from.clone().filter(|f| !f.is_empty()).or_else(|| user.clone()).unwrap_or_else(|| format!("agent-hub@{}", c.host));
    let msg = Message::builder()
        .from(from.parse().map_err(|e| format!("smtp from: {e}"))?)
        .to(to.parse().map_err(|e| format!("smtp to: {e}"))?)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(text.to_string())
        .map_err(|e| format!("smtp message: {e}"))?;
    let mut b = if is_local(&c.host) && c.starttls == Some(false) {
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&c.host)
    } else if port == 465 && c.secure != Some(false) {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&c.host).map_err(|e| format!("smtp: {e}"))?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&c.host).map_err(|e| format!("smtp: {e}"))?
    }
    .port(port)
    .timeout(Some(std::time::Duration::from_secs(20)));
    if let Some(u) = user {
        b = b.credentials(Credentials::new(u, c.pass.clone().unwrap_or_default()));
    }
    b.build().send(msg).await.map(|_| ()).map_err(|e| format!("smtp: {e}"))
}

/// Where a request came from, for an alert: time, IP, device.
pub fn origin(app: &App, req: &Req) -> Vec<String> {
    let ip = req.ip(&app.cfg);
    vec![
        alert_time(app),
        format!("IP: {}", if ip.is_empty() { app.l("unknown", "未知") } else { ip }),
        format!("{}: {}", app.l("Device", "设备"), req.header("user-agent").map(|u| crate::app::truncate(u, 200)).unwrap_or_else(|| app.l("unknown", "未知"))),
    ]
}

pub fn alert_time(app: &App) -> String {
    let now = app.now();
    if app.cfg.en {
        format!("Time: {}", app.iso(now))
    } else {
        let t = chrono::DateTime::from_timestamp_millis(now).unwrap_or_default() + chrono::Duration::hours(8);
        format!("时间：{}（北京时间）", t.format("%Y/%-m/%-d %H:%M:%S"))
    }
}

/// A security event: put in the feed and mailed to every HUB_OWNER_EMAIL address. Never blocks or fails the request.
pub fn alert(app: &Arc<App>, c: &Connection, title: &str, lines: &[String]) {
    let body: Vec<&str> = lines.iter().map(String::as_str).filter(|l| !l.is_empty()).collect();
    crate::db::append(app, c, json!({ "from": "hub", "kind": "notice", "text": format!("🔐 {title}\n{}", body.join("\n")) }));
    let Some(smtp) = app.cfg.smtp.clone() else { return };
    let subject = app.l(&format!("Noop security alert: {title}"), &format!("Noop 安全提醒：{title}"));
    let text = format!("{subject}\n\n{}\n", lines.join("\n"));
    for to in app.owners().to_vec() {
        let (smtp, subject, text) = (smtp.clone(), subject.clone(), text.clone());
        tokio::spawn(async move {
            if let Err(e) = send(&smtp, &to, &subject, &text).await {
                tracing::error!("alert mail to {to}: {e}");
            }
        });
    }
}
