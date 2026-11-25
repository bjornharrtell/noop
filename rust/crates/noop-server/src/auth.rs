//! Owner sign-in (emailed code + TOTP), sessions, and the machine-only security routes. Port of the auth parts of
//! cloud/src/index.ts (Hub.auth, checkCode, newSession, who, /api/host/2fa*, login-code, join, revoke-sessions).
use std::sync::{Arc, LazyLock};

use axum::http::Method;
use axum::response::Response;
use noop_core::authcode::{code_ok, new_code, norm_code};
use noop_core::totp::{new_secret, otpauth_uri, totp_match};
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::app::{err, json, norm_email, ok, sha256hex, truncate, App, Req};
use crate::db::{kv_del, kv_get, kv_set};
use crate::mail::{alert, alert_time, origin};

const CODE_TTL: i64 = 10 * 60_000;
const CODE_TRIES: i64 = 5;
const RESEND_MS: i64 = 60_000;
const SENDS_PER_HOUR: i64 = 10;
const SEEN_EVERY: i64 = 3_600_000;
/// Wrong code guesses are capped per UTC day across every code; past the cap nobody can verify until 00:00 UTC.
const FAILS_PER_DAY: i64 = 20;
/// Owner sessions (app, browser, MCP access tokens) die after this long unused.
const SESSION_IDLE_MS: i64 = 30 * 86_400_000;

static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$").unwrap());
static SESSION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{64}$").unwrap());

fn valid_email(e: &str) -> bool {
    e.len() <= 254 && EMAIL_RE.is_match(e)
}

fn totp_missing(app: &App) -> String {
    app.l(
        "2FA (authenticator app) is not bound yet, nobody can log in: run noop 2fa setup on your host first",
        "还没有绑定两步验证（验证器 App），任何人都无法登录：先在你的主机上运行 noop 2fa setup",
    )
}

#[derive(Debug, Clone, PartialEq)]
pub enum Who {
    Machine,
    Session { id: String, mcp: bool },
}

/// `Authorization: Bearer …`, else `?token=` (WebSocket / <img> can't send headers).
pub fn credential(req: &Req) -> String {
    match req.header("authorization") {
        Some(h) if h.starts_with("Bearer ") => h[7..].trim().to_string(),
        _ => req.q("token").unwrap_or("").to_string(),
    }
}

/// Machine for HUB_TOKEN, the session for an owner session, None otherwise. A session whose email left the
/// allow-list, or that went unused for SESSION_IDLE_MS, is deleted on sight.
pub fn who(app: &App, c: &Connection, req: &Req) -> rusqlite::Result<Option<Who>> {
    let t = credential(req);
    if t.is_empty() {
        return Ok(None);
    }
    if noop_core::ct_eq(t.as_bytes(), app.cfg.hub_token.as_bytes()) {
        return Ok(Some(Who::Machine));
    }
    if !SESSION_RE.is_match(&t) {
        return Ok(None);
    }
    let id = sha256hex(t.as_bytes()); // only hashes of session ids are stored
    let r = c
        .query_row("SELECT last_seen, email, ua, scope FROM sessions WHERE id = ?", [&id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?))
        })
        .optional()?;
    let Some((last_seen, email, ua, scope)) = r else { return Ok(None) };
    let now = app.now();
    if !app.owner_ok(email.as_deref().unwrap_or("")) || now - last_seen > SESSION_IDLE_MS {
        c.execute("DELETE FROM sessions WHERE id = ?", [&id])?;
        c.execute("DELETE FROM push_subs WHERE session = ?", [&id])?;
        return Ok(None);
    }
    if now - last_seen > SEEN_EVERY {
        c.execute("UPDATE sessions SET last_seen = ? WHERE id = ?", params![now, id])?;
    }
    let mcp = scope.as_deref() == Some("mcp") || (scope.is_none() && ua.unwrap_or_default().starts_with("mcp:"));
    Ok(Some(Who::Session { id, mcp }))
}

/// The bound authenticator, or None (then nobody can sign in).
fn totp_state(c: &Connection) -> Option<Value> {
    kv_get(c, "totp").filter(|t| t.get("secret").and_then(Value::as_str).is_some_and(|s| !s.is_empty()))
}

pub fn new_session(app: &App, c: &Connection, email: &str, ua: &str) -> rusqlite::Result<Option<String>> {
    if !app.owner_ok(email) {
        return Ok(None);
    }
    let mut b = [0u8; 32];
    noop_core::random_bytes(&mut b);
    let session = hex::encode(b);
    let now = app.now();
    c.execute(
        "INSERT INTO sessions (id, email, created, last_seen, ua, scope) VALUES (?, ?, ?, ?, ?, ?)",
        params![sha256hex(session.as_bytes()), email, now, now, truncate(ua, 300), if ua.starts_with("mcp:") { "mcp" } else { "owner" }],
    )?;
    Ok(Some(session))
}

pub struct CodeCheck {
    pub email: Option<String>,
    pub error: String,
    pub status: u16,
}

/// The emailed code AND the authenticator's TOTP. A success is a new sign-in: the owners are told by email and in the
/// feed, so a sign-in they didn't make is noticed at once. `via` is set for interactive sign-ins (alerts are sent).
pub fn check_code(app: &Arc<App>, c: &Connection, email: &str, code: &str, totp: &str, ctx: Option<(&Req, &str)>) -> rusqlite::Result<CodeCheck> {
    let fail_with = |error: String, status: u16| CodeCheck { email: None, error, status };
    let email = norm_email(email);
    let code = norm_code(code);
    let totp: String = totp.chars().filter(|c| !c.is_whitespace()).collect();
    let Some(tf) = totp_state(c) else { return Ok(fail_with(totp_missing(app), 403)) };
    if !valid_email(&email) || !code_ok(&code) || !(totp.len() == 6 && totp.bytes().all(|b| b.is_ascii_digit())) {
        return Ok(fail_with(app.l("Enter the email code and the 2FA code", "请填写邮箱验证码和两步验证码"), 400));
    }
    let now = app.now();
    let r = c
        .query_row("SELECT hash, expires, attempts FROM auth_codes WHERE email = ?", [&email], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })
        .optional()?;
    let Some((hash, _, attempts)) = r.filter(|(_, expires, _)| *expires >= now) else {
        return Ok(fail_with(app.l("The code is invalid or expired, request a new one", "验证码无效或已过期，请重新获取"), 401));
    };
    if attempts >= CODE_TRIES {
        return Ok(fail_with(app.l("Too many attempts, request a new code", "尝试次数太多，请重新获取验证码"), 401));
    }
    let day = &app.iso(now)[..10];
    let failed = kv_get(c, "auth_fails")
        .filter(|f| f.get("day").and_then(Value::as_str) == Some(day))
        .and_then(|f| f.get("n").and_then(Value::as_i64))
        .unwrap_or(0);
    if failed >= FAILS_PER_DAY {
        return Ok(fail_with(app.l("Too many failed verifications today, locked until 00:00 UTC", "今天验证失败次数太多，已锁定到 UTC 0 点"), 429));
    }
    c.execute("UPDATE auth_codes SET attempts = attempts + 1 WHERE email = ?", [&email])?;
    let fail = |error: String| -> rusqlite::Result<CodeCheck> {
        kv_set(c, "auth_fails", &json!({ "day": day, "n": failed + 1 }))?;
        if failed + 1 >= FAILS_PER_DAY {
            c.execute("DELETE FROM auth_codes", [])?;
            let mut lines = vec![app.l(
                &format!("{FAILS_PER_DAY} failed verifications today, nobody can log in until 00:00 UTC."),
                &format!("今天验证已经错了 {FAILS_PER_DAY} 次，到 UTC 0 点前任何人都无法登录。"),
            )];
            if let Some((req, _)) = ctx {
                lines.extend(origin(app, req));
            }
            lines.push(String::new());
            lines.push(app.l("If this was not you, someone is guessing your codes. Existing logins are not affected.", "如果不是你本人在试：有人在猜你的验证码。已有的登录不受影响。"));
            alert(app, c, &app.l("Login locked", "登录已锁定"), &lines);
        }
        Ok(fail_with(error, 401))
    };
    if !noop_core::ct_eq(sha256hex(format!("{email}:{code}").as_bytes()).as_bytes(), hash.as_bytes()) {
        return fail(app.l("The email code is wrong", "邮箱验证码不对"));
    }
    // the email code was right: a wrong TOTP keeps it (within its CODE_TRIES), so the owner can retype just the TOTP
    let secret = tf["secret"].as_str().unwrap_or_default();
    let last = tf.get("last").and_then(Value::as_i64).unwrap_or(-1);
    let Some(step) = totp_match(secret, &totp, last, now) else {
        if let Some((req, _)) = ctx {
            let mut lines = vec![app.l(
                &format!("Someone entered the correct email code for {email}, but the wrong 2FA code."),
                &format!("有人输对了 {email} 的邮箱验证码，但两步验证码不对。"),
            )];
            lines.extend(origin(app, req));
            lines.push(String::new());
            lines.push(app.l("If this was not you, your mailbox may be compromised: change its password now.", "如果不是你本人：你的邮箱可能已经被盗，请立即修改邮箱密码。"));
            alert(app, c, &app.l("Wrong 2FA code", "两步验证码错误"), &lines);
        }
        return fail(app.l(
            "The 2FA code is wrong (or already used), check the latest number in your authenticator app",
            "两步验证码不对（或已用过），请看验证器 App 上的最新数字",
        ));
    };
    let mut tf = tf.clone();
    tf["last"] = json!(step); // each code works once
    kv_set(c, "totp", &tf)?;
    c.execute("DELETE FROM auth_codes WHERE email = ?", [&email])?;
    kv_del(c, "cli_code")?;
    if let Some((req, via)) = ctx {
        let mut lines = vec![app.l(&format!("Account: {email}"), &format!("账号：{email}")), app.l(&format!("Method: {via}"), &format!("方式：{via}"))];
        lines.extend(origin(app, req));
        lines.push(String::new());
        lines.push(app.l("If this was you, nothing to do.", "如果是你本人，无需处理。"));
        lines.push(app.l(
            "If this was not you: run noop revoke-sessions on your host to sign out everything at once (phones, browsers, MCP connectors), then check whether this mailbox is compromised.",
            "如果不是你：在你的主机上运行 noop revoke-sessions，立即踢掉所有登录（手机、浏览器、MCP 连接器），再检查这个邮箱是否被盗。",
        ));
        alert(app, c, &app.l("New login", "新登录"), &lines);
    }
    Ok(CodeCheck { email: Some(email), error: String::new(), status: 200 })
}

/// POST /api/auth/start | verify | logout
pub async fn auth(app: &Arc<App>, req: &Req) -> rusqlite::Result<Response> {
    if req.method != Method::POST {
        return Ok(err("method not allowed", 405));
    }
    let body = req.json().unwrap_or(Value::Null);
    let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let now = app.now();
    match req.path.as_str() {
        "/api/auth/start" => {
            let email = norm_email(&s("email"));
            if !valid_email(&email) {
                return Ok(err("invalid email", 400));
            }
            let not_allowed = || err(app.l("This email is not allowed to log in", "这个邮箱不允许登录"), 403);
            let owners = app.owners().to_vec();
            // only the allow-list may sign in: anyone else is told so at once, with no code, no mail and no row written
            if !owners.is_empty() && !owners.contains(&email) {
                return Ok(not_allowed());
            }
            let code;
            {
                let c = app.db();
                if totp_state(&c).is_none() {
                    return Ok(json(json!({ "error": "totp_not_setup", "message": totp_missing(app) }), 403));
                }
                // no email set up yet: the code is shown on the owner's own machine instead (`noop login`)
                let cli = app.cfg.smtp.is_none();
                c.execute("DELETE FROM auth_sends WHERE ts < ?", [now - 3_600_000])?;
                let recent: i64 = c.query_row("SELECT COUNT(*) FROM auth_sends WHERE email = ? AND ts > ?", params![email, now - RESEND_MS], |r| r.get(0))?;
                if recent > 0 {
                    return Ok(err(app.l("Wait a minute before requesting another code", "请稍等一分钟再重新获取验证码"), 429));
                }
                let sent: i64 = c.query_row("SELECT COUNT(*) FROM auth_sends WHERE sent = 1", [], |r| r.get(0))?;
                if sent >= SENDS_PER_HOUR {
                    return Ok(err(app.l("Too many codes sent, try again in an hour", "验证码发送太频繁，请一小时后再试"), 429));
                }
                let is_owner = owners.contains(&email) || (cli && owners.is_empty());
                c.execute("INSERT INTO auth_sends (email, ts, sent) VALUES (?, ?, ?)", params![email, now, is_owner as i64])?;
                if !is_owner {
                    return Ok(not_allowed()); // no allow-list and email set up: nobody is the owner
                }
                code = new_code();
                c.execute(
                    "INSERT OR REPLACE INTO auth_codes (email, hash, expires, attempts) VALUES (?, ?, ?, 0)",
                    params![email, sha256hex(format!("{email}:{code}").as_bytes()), now + CODE_TTL],
                )?;
                if cli {
                    kv_set(&c, "cli_code", &json!({ "email": email, "code": code, "expires": now + CODE_TTL }))?;
                    return Ok(ok(json!({ "ok": true, "delivery": "cli" })));
                }
            }
            let smtp = app.cfg.smtp.clone().expect("checked above");
            let subject = app.l(&format!("Noop login code: {code}"), &format!("Noop 登录验证码：{code}"));
            let text = app.l(
                &format!("Your Noop login code is:\n\n    {code}\n\nValid for 10 minutes.\n\nIf this was not you, ignore this email.\n"),
                &format!("你的 Noop 登录验证码是：\n\n    {code}\n\n10 分钟内有效。\n\n如果不是你本人操作，请忽略这封邮件。\n"),
            );
            if let Err(e) = crate::mail::send(&smtp, &email, &subject, &text).await {
                tracing::error!("auth mail failed: {e}");
                app.db().execute("DELETE FROM auth_codes WHERE email = ?", [&email])?;
                return Ok(err(app.l("Sending the code email failed, try again later", "验证码邮件发送失败，请稍后再试"), 502));
            }
            Ok(ok(json!({ "ok": true })))
        }
        "/api/auth/verify" => {
            let c = app.db();
            let via = app.l("App / browser login", "App / 浏览器登录");
            let r = check_code(app, &c, &s("email"), &s("code"), &s("totp"), Some((req, &via)))?;
            let Some(email) = r.email else { return Ok(err(r.error, r.status)) };
            match new_session(app, &c, &email, req.header("user-agent").unwrap_or(""))? {
                Some(session) => Ok(ok(json!({ "session": session }))),
                None => Ok(err("not an owner", 403)),
            }
        }
        "/api/auth/logout" => {
            let c = app.db();
            if let Some(Who::Session { id, .. }) = who(app, &c, req)? {
                c.execute("DELETE FROM sessions WHERE id = ?", [&id])?;
                c.execute("DELETE FROM push_subs WHERE session = ?", [&id])?;
            }
            Ok(ok(json!({ "ok": true })))
        }
        _ => Ok(err("not found", 404)),
    }
}

/// Machine-only security routes and the owner's machine join. `None` when the path is not one of them.
pub fn host_routes(app: &Arc<App>, c: &Connection, req: &Req, w: &Who) -> rusqlite::Result<Option<Response>> {
    let p = req.path.as_str();
    let m = &req.method;
    let machine = *w == Who::Machine;
    let host = || req.header("x-noop-host").map(str::to_string);
    // Two-factor binding, from a machine only (the machine key is the root of trust; a stolen mailbox can't bind)
    if p.starts_with("/api/host/2fa") {
        if !machine {
            return Ok(Some(err("forbidden", 403)));
        }
        let by = host().unwrap_or_else(|| app.l("a host holding the machine key", "持有机器密钥的主机"));
        if p == "/api/host/2fa" && m == Method::GET {
            let t = totp_state(c);
            // also what `noop security` can't see from config.toml: the live allow-list
            let allow = app.cfg.allow_ips.as_ref().map(Vec::len);
            return Ok(Some(ok(json!({ "bound": t.is_some(), "since": t.and_then(|t| t.get("bound").cloned()), "allow_ips": allow }))));
        }
        if p == "/api/host/2fa/setup" && m == Method::POST {
            let secret = new_secret();
            kv_set(c, "totp_pending", &json!({ "secret": secret, "expires": app.now() + 10 * 60_000 }))?;
            let account = app.owners().first().cloned().unwrap_or_else(|| "owner".into());
            return Ok(Some(ok(json!({ "secret": secret, "uri": otpauth_uri(&secret, &account, "Noop") }))));
        }
        if p == "/api/host/2fa/confirm" && m == Method::POST {
            let code = req.json().and_then(|b| b.get("totp").and_then(Value::as_str).map(str::to_string)).unwrap_or_default();
            let pend = kv_get(c, "totp_pending");
            let secret = pend
                .as_ref()
                .filter(|p| p.get("expires").and_then(Value::as_i64).unwrap_or(0) >= app.now())
                .and_then(|p| p.get("secret").and_then(Value::as_str))
                .map(str::to_string);
            let Some(secret) = secret else {
                return Ok(Some(err(app.l("No binding waiting for confirmation (valid for 10 minutes), run noop 2fa setup again", "没有待确认的绑定（10 分钟内有效），重新运行 noop 2fa setup"), 400)));
            };
            let Some(step) = totp_match(&secret, &code, -1, app.now()) else {
                return Ok(Some(err(app.l("The 2FA code is wrong: check your phone's clock and enter the latest number in the authenticator app", "两步验证码不对：确认手机时间准确，输入验证器 App 上的最新数字"), 400)));
            };
            let was = totp_state(c).is_some();
            kv_set(c, "totp", &json!({ "secret": secret, "last": step, "bound": app.now() }))?;
            kv_del(c, "totp_pending")?;
            let title = if was { app.l("2FA re-bound", "两步验证已换绑") } else { app.l("2FA turned on", "两步验证已开启") };
            let note = if was {
                app.l("The old authenticator no longer works. If this was not you, someone has your machine key: check your host now.", "旧的验证器从现在起失效。如果不是你本人操作：有人拿到了你的机器密钥，立即检查你的主机。")
            } else {
                app.l("From now on, logging in needs the email code and the 2FA code from your authenticator app.", "从现在起，登录需要邮箱验证码 + 验证器 App 上的两步验证码。")
            };
            alert(app, c, &title, &[app.l(&format!("By: {by}"), &format!("操作方：{by}")), alert_time(app), String::new(), note]);
            return Ok(Some(ok(json!({ "ok": true, "replaced": was }))));
        }
        return Ok(Some(err("not found", 404)));
    }
    if p == "/api/host/login-code" {
        // `noop login` on the owner's machine, when email isn't configured
        if !machine {
            return Ok(Some(err("forbidden", 403)));
        }
        return Ok(Some(match kv_get(c, "cli_code").filter(|v| v.get("expires").and_then(Value::as_i64).unwrap_or(0) > app.now()) {
            Some(v) => ok(v),
            None => err("no pending code", 404),
        }));
    }
    if p == "/api/host/join" && m == Method::POST {
        // a new machine, approved by the owner's login, gets the machine key
        if machine {
            return Ok(Some(err("forbidden", 403)));
        }
        let name = req.json().and_then(|b| b.get("host").and_then(Value::as_str).map(|h| truncate(h, 60))).unwrap_or_else(|| "?".into());
        crate::db::append(app, c, json!({ "from": "hub", "kind": "notice", "text": app.l(&format!("New machine {name} joined the team."), &format!("新机器 {name} 已加入团队。")) }));
        return Ok(Some(ok(json!({ "token": app.cfg.hub_token }))));
    }
    if p == "/api/host/revoke-sessions" && m == Method::POST {
        if !machine {
            return Ok(Some(err("forbidden", 403)));
        }
        let n: i64 = c.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        // connectors too: a refresh token would otherwise mint a fresh session right back
        c.execute_batch("DELETE FROM sessions; DELETE FROM push_subs; DELETE FROM oauth_refresh; DELETE FROM oauth_codes; DELETE FROM auth_codes;")?;
        let by = host().unwrap_or_else(|| app.l("a host holding the machine key", "持有机器密钥的主机"));
        alert(app, c, &app.l("All logins revoked", "所有登录已撤销"), &[
            app.l(&format!("{n} login(s) (phones, browsers, MCP connectors) were signed out and need to log in again with an email code."), &format!("{n} 个登录（手机、浏览器、MCP 连接器）已被踢下线，需要重新用邮箱验证码登录。")),
            app.l(&format!("By: {by}"), &format!("操作方：{by}")),
        ]);
        return Ok(Some(ok(json!({ "ok": true, "revoked": n }))));
    }
    Ok(None)
}
