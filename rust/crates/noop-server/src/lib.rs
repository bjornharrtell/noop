//! Noop server: a self-hosted replacement for the Cloudflare Worker (cloud/src), same HTTP API.
//!
//! Ported so far: file sync (/api/files/*), owner sign-in (/api/auth/*), sessions, 2FA binding and the machine
//! security routes (/api/host/2fa*, login-code, join, revoke-sessions). See rust/README.md for what is left.
pub mod app;
pub mod auth;
pub mod db;
pub mod feed;
pub mod files;
pub mod mail;
pub mod media;
pub mod push;
pub mod stream;
pub mod web;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::Method;
use axum::response::Response;
use axum::Router;

use app::{err, App, Req};
use auth::Who;

/// Largest request body: a commit carries base64 files of up to 1.5 MB each.
const MAX_BODY: usize = 100 * 1024 * 1024;

pub fn router(app: Arc<App>) -> Router {
    Router::new().fallback(handle).with_state(app)
}

async fn handle(State(app): State<Arc<App>>, req: Request) -> Response {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let (mut parts, body) = req.into_parts();
    let upgrade = parts.headers.get("upgrade").and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let ws = if upgrade { WebSocketUpgrade::from_request_parts(&mut parts, &app).await.ok() } else { None };
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY).await else { return err("request too large", 413) };
    let req = Req {
        method: parts.method,
        path: parts.uri.path().to_string(),
        query: app::parse_query(parts.uri.query()),
        headers: parts.headers,
        body,
        peer,
        ws: std::sync::Mutex::new(ws),
    };
    match route(&app, &req).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("{} {}: {e}", req.method, req.path);
            err("internal error", 500)
        }
    }
}

async fn route(app: &Arc<App>, req: &Req) -> rusqlite::Result<Response> {
    let p = req.path.as_str();
    let mcp = p == "/mcp" || p.starts_with("/oauth/") || p.starts_with("/.well-known/oauth-") || p == "/.well-known/openid-configuration";
    if mcp {
        // TODO(rust-port): MCP server and OAuth 2.1 provider
        return Ok(err("not found", 404));
    }
    if !p.starts_with("/api/") && !p.starts_with("/media/") {
        return Ok(web::serve(app, req).await);
    }
    if p.starts_with("/api/auth/") {
        return auth::auth(app, req).await;
    }
    // TODO(rust-port): guest workers (/api/guest/join, guest tokens), /api/mcp-oauth/callback
    let Some(w) = auth::who(app, &app.db(), req)? else { return Ok(err("unauthorized", 401)) };
    // a connector's OAuth token is for /mcp only: never the owner API
    if matches!(w, Who::Session { mcp: true, .. }) {
        return Ok(err("forbidden", 403));
    }
    if p == "/api/host/inbox" {
        // waits without holding the database
        return feed::inbox(app, req).await;
    }
    if p == "/api/stream" || p == "/api/host/stream" {
        return Ok(stream::open(app, req, &w));
    }
    // reads the multipart body before taking the database
    if let Some(r) = media::routes(app, req).await? {
        return Ok(r);
    }
    if let Some(r) = push::routes(app, req, &w).await? {
        return Ok(r);
    }
    let mut c = app.db();
    let machine = w == Who::Machine;
    if (p == "/api/files/list" || p == "/api/files/read") && req.method == Method::GET {
        if let Some(r) = files::handle_read(&c, req)? {
            return Ok(r);
        }
    }
    if p.starts_with("/api/files/") {
        let owner_only = p == "/api/files/history" || p == "/api/files/revert";
        if if owner_only { machine } else { !machine } {
            return Ok(err("forbidden", 403));
        }
        let actor = if machine { req.header("x-noop-host").unwrap_or("machine").to_string() } else { "owner".to_string() };
        // TODO(rust-port): MCP resources/updated for changed tasks and decisions
        if let Some(r) = files::handle(&mut c, req, &actor, app.now())? {
            return Ok(r);
        }
    }
    if let Some(r) = auth::host_routes(app, &c, req, &w)? {
        return Ok(r);
    }
    if let Some(r) = feed::routes(app, &c, req, &w)? {
        return Ok(r);
    }
    Ok(err("not found", 404))
}

/// Serve until the process is stopped.
pub async fn serve(app: Arc<App>, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("noop-server listening on {addr}");
    serve_on(app, listener).await
}

/// Serve on a bound listener (tests bind port 0).
pub async fn serve_on(app: Arc<App>, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    axum::serve(listener, router(app).into_make_service_with_connect_info::<SocketAddr>()).await
}
