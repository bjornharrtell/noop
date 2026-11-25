//! Signing in to a remote MCP server once, for every machine (Settings → MCP → a server's "Sign in"). Port of
//! cloud/src/mcp-oauth.ts and its routes:
//!
//!   GET    /api/mcp-oauth                 → {servers: [{name, signed_in, expires_at?, renewable, updated}]}  never a token
//!   POST   /api/mcp-oauth/start {name}    (owner) → {url}   the authorization page to open
//!   GET    /api/mcp-oauth/callback        (no session: the one-time state is the credential) → a small page
//!   DELETE /api/mcp-oauth?name=           (owner) sign out
//!
//! MCP authorization (spec 2025-06-18): protected-resource metadata (RFC 9728) → authorization-server metadata
//! (RFC 8414 / OpenID discovery) → dynamic registration (RFC 7591) → authorization code with PKCE S256 and `resource`
//! (RFC 8707). Tokens are sealed like the shared secrets (name "mcp-oauth:<server>"); the server alone holds the
//! refresh token and refreshes before handing out an access token, so machines never race over a rotating one.
//! Endpoints must be https://, except on this machine (127.0.0.1 / localhost), which tests use.
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::Method;
use axum::response::Response;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use regex::Regex;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::app::{err, ok, App, Req};
use crate::auth::Who;
use crate::secrets::{seal, unseal};

/// A token that ends within this is refreshed before it is handed out (agent runs last up to an hour).
pub const REFRESH_EARLY_MS: i64 = 15 * 60_000;
/// A started sign-in that is not finished within this is dropped.
pub const PENDING_TTL_MS: i64 = 10 * 60_000;

static MCP_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][\w-]{0,39}$").unwrap());

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Client {
    pub client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Tokens {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}
/// Everything kept per server (sealed); the same JSON shape as the Worker's OAuthState.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct State {
    pub meta: Meta,
    pub client: Client,
    pub resource: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
}

/// The environment variable a machine gets the access token in.
pub fn oauth_var(server: &str) -> String {
    let s: String = server.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    format!("NOOP_OAUTH_{}", s.to_uppercase())
}

fn allowed(u: &str) -> bool {
    u.starts_with("https://") || ["http://127.0.0.1:", "http://127.0.0.1/", "http://localhost:", "http://localhost/"].iter().any(|p| u.starts_with(p))
}

fn b64url(b: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(b)
}
pub fn random_token(n: usize) -> String {
    let mut b = vec![0u8; n];
    noop_core::random_bytes(&mut b);
    b64url(&b)
}
pub fn pkce_challenge(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

async fn get_json(http: &reqwest::Client, url: &str) -> Option<Value> {
    let r = http
        .get(url)
        .header("accept", "application/json")
        .header("mcp-protocol-version", "2025-06-18")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json().await.ok()
}

/// `resource_metadata="…"` of a 401's WWW-Authenticate.
pub fn resource_metadata_url(header: &str) -> Option<String> {
    let i = header.find("resource_metadata=\"")? + "resource_metadata=\"".len();
    let rest = &header[i..];
    Some(rest[..rest.find('"')?].to_string())
}

fn origin_and_path(u: &reqwest::Url) -> (String, String) {
    (u.origin().ascii_serialization(), u.path().to_string())
}

/// Find the authorization server of an MCP server and its endpoints. None: it does not do OAuth.
pub async fn discover(http: &reqwest::Client, server_url: &str) -> Option<(Meta, String)> {
    let u = reqwest::Url::parse(server_url).ok()?;
    let (origin, path) = origin_and_path(&u);
    // 1. protected-resource metadata: from the 401's header, else the well-known paths (path-aware first)
    let probe = http
        .post(server_url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "noop", "version": "1" } } }).to_string())
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok();
    let hinted = probe
        .filter(|r| r.status() == 401)
        .and_then(|r| r.headers().get("www-authenticate").and_then(|h| h.to_str().ok()).and_then(resource_metadata_url));
    let candidates = [
        hinted,
        Some(format!("{origin}/.well-known/oauth-protected-resource{}", if path == "/" { "" } else { &path })),
        Some(format!("{origin}/.well-known/oauth-protected-resource")),
    ];
    let mut prm = None;
    for m in candidates.into_iter().flatten() {
        if let Some(v) = get_json(http, &m).await {
            prm = Some(v);
            break;
        }
    }
    let prm = prm.unwrap_or(Value::Null);
    let issuer = prm.pointer("/authorization_servers/0").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| origin.clone());
    let resource = prm.get("resource").and_then(Value::as_str).unwrap_or(server_url).to_string();
    // 2. authorization-server metadata (RFC 8414 path insertion, then OpenID discovery)
    let iu = reqwest::Url::parse(&issuer).ok()?;
    let (io, ip) = origin_and_path(&iu);
    let ip = if ip == "/" { String::new() } else { ip.trim_end_matches('/').to_string() };
    let mut meta = None;
    for m in [
        format!("{io}/.well-known/oauth-authorization-server{ip}"),
        format!("{io}/.well-known/openid-configuration{ip}"),
        format!("{io}{ip}/.well-known/openid-configuration"),
    ] {
        if let Some(v) = get_json(http, &m).await {
            meta = Some(v);
            break;
        }
    }
    let m = meta?;
    let s = |k: &str| m.get(k).and_then(Value::as_str).filter(|u| allowed(u)).map(str::to_string);
    Some((
        Meta {
            authorization_endpoint: s("authorization_endpoint")?,
            token_endpoint: s("token_endpoint")?,
            registration_endpoint: s("registration_endpoint"),
            scopes_supported: m.get("scopes_supported").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()),
        },
        resource,
    ))
}

/// Dynamic client registration; None when the server does not offer it.
pub async fn register(http: &reqwest::Client, meta: &Meta, redirect: &str) -> Option<Client> {
    let ep = meta.registration_endpoint.as_ref()?;
    let r = http
        .post(ep)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(json!({ "client_name": "Noop", "redirect_uris": [redirect], "grant_types": ["authorization_code", "refresh_token"], "response_types": ["code"], "token_endpoint_auth_method": "none" }).to_string())
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    let j: Value = r.json().await.ok()?;
    Some(Client { client_id: j.get("client_id")?.as_str()?.to_string(), client_secret: j.get("client_secret").and_then(Value::as_str).map(String::from) })
}

pub fn authorize_url(s: &State, redirect: &str, state: &str, challenge: &str) -> String {
    let mut u = reqwest::Url::parse(&s.meta.authorization_endpoint).expect("checked by discover");
    {
        let mut q = u.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &s.client.client_id)
            .append_pair("redirect_uri", redirect)
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", &s.resource);
        if let Some(scopes) = s.meta.scopes_supported.as_ref().filter(|s| !s.is_empty()) {
            q.append_pair("scope", &scopes.join(" "));
        }
    }
    u.to_string()
}

/// POST to the token endpoint; None on any failure. Keeps the old refresh token when the server does not rotate it.
pub async fn token_request(http: &reqwest::Client, s: &State, params: &[(&str, &str)], now: i64) -> Option<Tokens> {
    let mut form: Vec<(&str, &str)> = params.to_vec();
    form.push(("client_id", &s.client.client_id));
    form.push(("resource", &s.resource));
    if let Some(secret) = &s.client.client_secret {
        form.push(("client_secret", secret));
    }
    let body = form.iter().map(|(k, v)| format!("{}={}", noop_core::encode_uri_component(k), noop_core::encode_uri_component(v))).collect::<Vec<_>>().join("&");
    let r = http
        .post(&s.meta.token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body(body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    let j: Value = r.json().await.ok()?;
    Some(Tokens {
        access_token: j.get("access_token")?.as_str()?.to_string(),
        refresh_token: j.get("refresh_token").and_then(Value::as_str).map(String::from).or_else(|| s.tokens.as_ref().and_then(|t| t.refresh_token.clone())),
        expires_at: j.get("expires_in").and_then(Value::as_f64).map(|e| now + (e * 1000.0) as i64),
        scope: j.get("scope").and_then(Value::as_str).map(String::from),
    })
}

/// The tokens to hand out now (refreshed when they end soon), and whether they changed. None: ended, not renewable.
pub async fn fresh_tokens(http: &reqwest::Client, s: &State, now: i64) -> (Option<Tokens>, bool) {
    let Some(t) = &s.tokens else { return (None, false) };
    match t.expires_at {
        None => return (Some(t.clone()), false),
        Some(e) if e - now > REFRESH_EARLY_MS => return (Some(t.clone()), false),
        _ => {}
    }
    if let Some(rt) = &t.refresh_token {
        if let Some(n) = token_request(http, s, &[("grant_type", "refresh_token"), ("refresh_token", rt)], now).await {
            return (Some(n), true);
        }
    }
    (t.expires_at.filter(|&e| e > now).map(|_| t.clone()), false)
}

/// The small page the browser lands on after signing in.
pub fn callback_page(ok: bool, title: &str, text: &str) -> Response {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{}</title>\n<style>body{{font:15px/1.5 system-ui,sans-serif;display:grid;place-items:center;min-height:90vh;margin:0;color:#222}}@media(prefers-color-scheme:dark){{body{{background:#161616;color:#eee}}}}main{{max-width:360px;padding:24px}}h1{{font-size:18px;margin:0 0 6px}}p{{margin:0;opacity:.7}}</style>\n<main><h1>{}{}</h1><p>{}</p></main>",
        esc(title), if ok { "✓ " } else { "" }, esc(title), esc(text)
    );
    Response::builder()
        .status(if ok { 200 } else { 400 })
        .header("content-type", "text/html; charset=utf-8")
        .header("cache-control", "no-store")
        .header("referrer-policy", "no-referrer")
        .body(Body::from(html))
        .expect("valid response")
}

// ---------------------------------------------------------------- storage

fn read(app: &App, name: &str) -> rusqlite::Result<Option<State>> {
    let sealed: Option<String> = app.db().query_row("SELECT sealed FROM mcp_oauth WHERE name = ?", [name], |r| r.get(0)).optional()?;
    Ok(sealed.and_then(|s| unseal(&app.cfg.hub_token, &format!("mcp-oauth:{name}"), &s)).and_then(|v| serde_json::from_str(&v).ok()))
}

fn write(app: &App, name: &str, s: &State) -> rusqlite::Result<()> {
    let sealed = seal(&app.cfg.hub_token, &format!("mcp-oauth:{name}"), &serde_json::to_string(s).unwrap_or_default());
    app.db().execute("INSERT OR REPLACE INTO mcp_oauth (name, sealed, updated) VALUES (?, ?, ?)", params![name, sealed, app.iso(app.now())])?;
    Ok(())
}

pub fn names(app: &App) -> rusqlite::Result<Vec<String>> {
    app.db().prepare_cached("SELECT name FROM mcp_oauth")?.query_map([], |r| r.get(0))?.collect()
}

/// The access token to hand a machine now (refreshed when it ends soon), or None.
pub async fn token(app: &Arc<App>, name: &str) -> rusqlite::Result<Option<String>> {
    let Some(s) = read(app, name)? else { return Ok(None) };
    let (tokens, changed) = fresh_tokens(&app.http, &s, app.now()).await;
    if changed {
        if let Some(t) = &tokens {
            write(app, name, &State { tokens: Some(t.clone()), ..s })?;
        }
    }
    Ok(tokens.map(|t| t.access_token))
}

/// What the app shows: signed in or not, until when; never a token.
fn list(app: &App) -> rusqlite::Result<Vec<Value>> {
    let rows: Vec<(String, Option<String>)> =
        app.db().prepare_cached("SELECT name, updated FROM mcp_oauth ORDER BY name")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let now = app.now();
    let mut out = Vec::new();
    for (name, updated) in rows {
        let t = read(app, &name)?.and_then(|s| s.tokens);
        let signed_in = t.as_ref().is_some_and(|t| t.expires_at.is_none_or(|e| e > now) || t.refresh_token.is_some());
        let mut v = json!({ "name": name, "signed_in": signed_in, "renewable": t.as_ref().is_some_and(|t| t.refresh_token.is_some()), "updated": updated });
        if let Some(e) = t.and_then(|t| t.expires_at) {
            v["expires_at"] = e.into();
        }
        out.push(v);
    }
    Ok(out)
}

/// The remote (http) MCP server with this name in settings.current.mcp_servers.
fn http_server(app: &App, name: &str) -> Option<String> {
    let settings = crate::db::kv_get(&app.db(), "settings")?;
    settings.pointer("/current/mcp_servers")?.as_array()?.iter().find_map(|s| {
        let n = s.get("name")?.as_str()?;
        let url = s.get("url")?.as_str()?.trim();
        (n == name && MCP_NAME.is_match(n) && s.get("type")?.as_str()? == "http" && (url.starts_with("https://") || url.starts_with("http://")))
            .then(|| url.to_string())
    })
}

/// The address the owner reached this server at (for the redirect URI).
fn origin(req: &Req) -> String {
    let proto = req.header("x-forwarded-proto").unwrap_or("http");
    let host = req.header("x-forwarded-host").or(req.header("host")).unwrap_or("localhost");
    format!("{proto}://{host}")
}

/// GET /api/mcp-oauth/callback: the browser lands here after signing in. No session; the one-time state is the
/// credential.
pub async fn callback(app: &Arc<App>, req: &Req) -> rusqlite::Result<Response> {
    let fail = |text: String| callback_page(false, &app.l("Sign-in failed", "登录失败"), &text);
    let state = req.q("state").unwrap_or("").to_string();
    let row: Option<(String, String, i64)> = {
        let c = app.db();
        let row = c.query_row("SELECT name, verifier, created FROM mcp_oauth_pending WHERE state = ?", [&state], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
        c.execute("DELETE FROM mcp_oauth_pending WHERE state = ?", [&state])?; // one use
        row
    };
    let Some((name, verifier, _)) = row.filter(|r| app.now() - r.2 <= PENDING_TTL_MS) else {
        return Ok(fail(app.l("This sign-in link expired. Start again from Noop.", "登录链接已失效，请回到 Noop 重新开始。")));
    };
    let Some(code) = req.q("code") else {
        let why = req.q("error_description").or(req.q("error")).map(str::to_string).unwrap_or_else(|| app.l("No authorization code", "没有收到授权码"));
        return Ok(fail(why));
    };
    let Some(s) = read(app, &name)? else { return Ok(fail(app.l("Start again from Noop.", "请回到 Noop 重新开始。"))) };
    let redirect = format!("{}/api/mcp-oauth/callback", origin(req));
    let params = [("grant_type", "authorization_code"), ("code", code), ("redirect_uri", redirect.as_str()), ("code_verifier", verifier.as_str())];
    let Some(tokens) = token_request(&app.http, &s, &params, app.now()).await else {
        return Ok(fail(app.l("The server did not issue a token.", "服务没有签发令牌。")));
    };
    write(app, &name, &State { tokens: Some(tokens), ..s })?;
    crate::db::append(app, &app.db(), json!({ "from": "hub", "kind": "notice",
        "text": app.l(&format!("Signed in to MCP server {name}; every machine uses it from its next task."), &format!("已登录 MCP 服务 {name}，各机器下一个任务起即可使用。")) }));
    Ok(callback_page(true, &app.l(&format!("Signed in to {name}"), &format!("已登录 {name}")), &app.l("Every machine can use it now. You can close this page.", "所有机器都可以使用了，可以关闭这个页面。")))
}

/// /api/mcp-oauth (list, start, sign out); None when the path is not one of them.
pub async fn routes(app: &Arc<App>, req: &Req, w: &Who) -> rusqlite::Result<Option<Response>> {
    let machine = *w == Who::Machine;
    match (req.method.clone(), req.path.as_str()) {
        (Method::GET, "/api/mcp-oauth") => Ok(Some(ok(json!({ "servers": list(app)? })))),
        (Method::DELETE, "/api/mcp-oauth") => {
            if machine {
                return Ok(Some(err("forbidden", 403)));
            }
            app.db().execute("DELETE FROM mcp_oauth WHERE name = ?", [req.q("name").unwrap_or("")])?;
            Ok(Some(ok(json!({ "servers": list(app)? }))))
        }
        (Method::POST, "/api/mcp-oauth/start") => {
            if machine {
                return Ok(Some(err("forbidden", 403)));
            }
            let name = req.json().and_then(|b| b.get("name").and_then(Value::as_str).map(String::from)).unwrap_or_default();
            let Some(url) = http_server(app, &name) else {
                return Ok(Some(err(app.l("No remote MCP server with that name", "没有这个名称的远程 MCP 服务"), 404)));
            };
            let redirect = format!("{}/api/mcp-oauth/callback", origin(req));
            let Some((meta, resource)) = discover(&app.http, &url).await else {
                return Ok(Some(err(app.l("This server does not support signing in with OAuth", "这个服务不支持 OAuth 登录"), 422)));
            };
            let prev = read(app, &name)?;
            // reuse the registered client while the server's endpoints are unchanged
            let client = match prev.as_ref().filter(|p| p.meta.token_endpoint == meta.token_endpoint) {
                Some(p) => Some(p.client.clone()),
                None => register(&app.http, &meta, &redirect).await,
            };
            let Some(client) = client else {
                return Ok(Some(err(app.l("This server does not allow registering Noop as a client", "这个服务不允许 Noop 自动注册为客户端"), 422)));
            };
            let s = State { meta, resource, client, tokens: prev.and_then(|p| p.tokens) };
            write(app, &name, &s)?;
            let (state, verifier) = (random_token(32), random_token(48));
            {
                let c = app.db();
                c.execute("DELETE FROM mcp_oauth_pending WHERE created < ?", [app.now() - PENDING_TTL_MS])?;
                c.execute("INSERT INTO mcp_oauth_pending (state, name, verifier, created) VALUES (?, ?, ?, ?)", params![state, name, verifier, app.now()])?;
            }
            Ok(Some(ok(json!({ "url": authorize_url(&s, &redirect, &state, &pkce_challenge(&verifier)) }))))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helpers() {
        assert_eq!(oauth_var("git-hub.x"), "NOOP_OAUTH_GIT_HUB_X");
        assert_eq!(resource_metadata_url(r#"Bearer resource_metadata="https://a.b/.well-known/x", scope="y""#).as_deref(), Some("https://a.b/.well-known/x"));
        // RFC 7636 appendix B
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }
}
