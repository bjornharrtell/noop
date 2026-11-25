//! Shared secrets, MCP server sign-in (against a fake OAuth server on localhost) and guest workers.
mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::*;
use serde_json::{json, Value};

#[tokio::test]
async fn secrets_are_one_way() {
    let t = T::new();
    let (session, _) = t.sign_in().await;
    let (s, r) = t.call("PUT", "/api/secrets", Some(&session), Some(json!({ "name": "GITHUB_TOKEN", "value": "ghp_secret" }))).await;
    assert_eq!(s, 200);
    assert_eq!(r["secrets"][0]["name"], "GITHUB_TOKEN");
    assert!(r.to_string().find("ghp_secret").is_none(), "the owner never reads a value back");
    // sealed at rest
    let sealed: String = t.app.db().query_row("SELECT sealed FROM secrets", [], |r| r.get(0)).unwrap();
    assert!(!sealed.contains("ghp_secret"));

    let (s, r) = t.machine("GET", "/api/secrets/values", None).await;
    assert_eq!((s, r["values"]["GITHUB_TOKEN"].as_str()), (200, Some("ghp_secret")));
    let (s, _) = t.call("GET", "/api/secrets/values", Some(&session), None).await;
    assert_eq!(s, 403, "a phone or browser cannot take the keys out");
    let (s, _) = t.machine("PUT", "/api/secrets", Some(json!({ "name": "X", "value": "y" }))).await;
    assert_eq!(s, 403);
    let (s, _) = t.call("PUT", "/api/secrets", Some(&session), Some(json!({ "name": "1bad", "value": "y" }))).await;
    assert_eq!(s, 400);
    let (_, r) = t.call("PUT", "/api/secrets", Some(&session), Some(json!({ "name": "GITHUB_TOKEN", "value": null }))).await;
    assert_eq!(r["secrets"], json!([]));
}

#[tokio::test]
async fn guest_lifecycle() {
    let t = T::new();
    let (session, _) = t.sign_in().await;
    let (s, r) = t.call("POST", "/api/guests/code", Some(&session), Some(json!({ "label": "test phone" }))).await;
    assert_eq!(s, 200);
    let code = r["code"].as_str().unwrap().to_string();

    let (s, j) = t.call("POST", "/api/guest/join", None, Some(json!({ "code": code }))).await;
    assert_eq!(s, 200);
    let (gid, token) = (j["id"].as_str().unwrap().to_string(), j["token"].as_str().unwrap().to_string());
    let (s, _) = t.call("POST", "/api/guest/join", None, Some(json!({ "code": code }))).await;
    assert_eq!(s, 401, "a code works once");

    // a guest token reaches nothing but its own API
    for (m, p) in [("GET", "/api/feed"), ("GET", "/api/files/changes?since=0"), ("GET", "/api/secrets/values"), ("GET", "/api/guests")] {
        let (s, _) = t.call(m, p, Some(&token), None).await;
        assert_eq!(s, 403, "{p}");
    }
    let (_, r) = t.call("GET", "/api/guest/next", Some(&token), None).await;
    assert_eq!(r["task"], Value::Null);

    let task = json!({ "guest": gid, "task": { "id": "T-0001", "title": "Check\u{0007} it", "body": "steps", "repo": "https://github.com/x/y" } });
    let (s, _) = t.call("POST", "/api/guests/assign", Some(&session), Some(task.clone())).await;
    assert_eq!(s, 403, "only machines assign");
    let (s, _) = t.machine("POST", "/api/guests/assign", Some(task)).await;
    assert_eq!(s, 200);
    let (_, r) = t.call("GET", "/api/guest/next", Some(&token), None).await;
    assert_eq!(r["task"], json!({ "id": "T-0001", "title": "Check it", "body": "steps", "repo": "https://github.com/x/y" }));

    let (s, _) = t.call("POST", "/api/guest/result", Some(&token), Some(json!({ "id": "T-9999", "ok": true, "text": "x" }))).await;
    assert_eq!(s, 403);
    let (s, _) = t.call("POST", "/api/guest/result", Some(&token), Some(json!({ "id": "T-0001", "ok": true, "text": "done", "patch": "diff" }))).await;
    assert_eq!(s, 200);
    let (_, r) = t.machine("GET", "/api/guests/results", None).await;
    assert_eq!(r["results"][0], json!({ "id": "T-0001", "guest": gid, "label": "test phone", "untrusted": true, "ok": true, "text": "done", "patch": "diff" }));
    t.machine("POST", "/api/guests/ack", Some(json!({ "ids": ["T-0001"] }))).await;
    let (_, r) = t.machine("GET", "/api/guests/results", None).await;
    assert_eq!(r["results"], json!([]));

    let (s, _) = t.call("POST", "/api/guests/revoke", Some(&session), Some(json!({ "id": gid }))).await;
    assert_eq!(s, 200);
    let (s, _) = t.call("GET", "/api/guest/next", Some(&token), None).await;
    assert_eq!(s, 401, "revoked");
}

/// A remote MCP server with its own OAuth authorization server.
async fn oauth_server() -> (String, Arc<Mutex<Vec<HashMap<String, String>>>>) {
    use axum::routing::{get, post};
    let forms: Arc<Mutex<Vec<HashMap<String, String>>>> = Arc::default();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let (b1, b2, b3) = (base.clone(), base.clone(), base.clone());
    let f2 = forms.clone();
    let app = axum::Router::new()
        .route("/mcp", post(move || {
            let b = b1.clone();
            async move { (axum::http::StatusCode::UNAUTHORIZED, [("www-authenticate", format!("Bearer resource_metadata=\"{b}/prm\""))]) }
        }))
        .route("/prm", get(move || {
            let b = b2.clone();
            async move { axum::Json(json!({ "resource": format!("{b}/mcp"), "authorization_servers": [b] })) }
        }))
        .route("/.well-known/oauth-authorization-server", get(move || {
            let b = b3.clone();
            async move {
                axum::Json(json!({ "authorization_endpoint": format!("{b}/authorize"), "token_endpoint": format!("{b}/token"),
                    "registration_endpoint": format!("{b}/register"), "scopes_supported": ["read"] }))
            }
        }))
        .route("/register", post(|| async { axum::Json(json!({ "client_id": "cid-1" })) }))
        .route("/token", post(move |body: String| {
            let forms = f2.clone();
            async move {
                let form: HashMap<String, String> = noop_server::app::parse_query(Some(&body)).into_iter().collect();
                forms.lock().unwrap().push(form.clone());
                match (form.get("grant_type").map(String::as_str), form.get("code").map(String::as_str), form.get("refresh_token").map(String::as_str)) {
                    (Some("authorization_code"), Some("good"), _) => axum::Json(json!({ "access_token": "a1", "refresh_token": "r1", "expires_in": 60 })).into_response_ok(),
                    (Some("refresh_token"), _, Some("r1")) => axum::Json(json!({ "access_token": "a2", "expires_in": 3600 })).into_response_ok(),
                    _ => (axum::http::StatusCode::BAD_REQUEST, "no").into_response_ok(),
                }
            }
        }));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (base, forms)
}

trait IntoOk {
    fn into_response_ok(self) -> axum::response::Response;
}
impl<T: axum::response::IntoResponse> IntoOk for T {
    fn into_response_ok(self) -> axum::response::Response {
        self.into_response()
    }
}

#[tokio::test]
async fn mcp_server_sign_in() {
    let t = T::new();
    let (session, _) = t.sign_in().await;
    let (base, forms) = oauth_server().await;
    t.call("PUT", "/api/settings", Some(&session), Some(json!({ "mcp_servers": [{ "name": "tracker", "type": "http", "url": format!("{base}/mcp") }] }))).await;

    let (s, r) = t.call("POST", "/api/mcp-oauth/start", Some(&session), Some(json!({ "name": "nope" }))).await;
    assert_eq!(s, 404, "{r}");
    let (s, r) = t.call("POST", "/api/mcp-oauth/start", Some(&session), Some(json!({ "name": "tracker" }))).await;
    assert_eq!(s, 200, "{r}");
    let url = reqwest::Url::parse(r["url"].as_str().unwrap()).unwrap();
    assert!(url.as_str().starts_with(&format!("{base}/authorize?")));
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!((q["client_id"].as_str(), q["code_challenge_method"].as_str(), q["scope"].as_str()), ("cid-1", "S256", "read"));
    assert_eq!(q["resource"], format!("{base}/mcp"));

    // the authorization server redirects the browser back with a code
    let (s, page, _) = t.raw("GET", &format!("/api/mcp-oauth/callback?state={}&code=good", q["state"]), &[]).await;
    assert_eq!(s, 200, "{page}");
    assert!(page.contains("Signed in to tracker"));
    let form = forms.lock().unwrap()[0].clone();
    assert_eq!(noop_server::mcp_oauth::pkce_challenge(&form["code_verifier"]), q["code_challenge"], "PKCE verifier matches the challenge");
    let (s, _, _) = t.raw("GET", &format!("/api/mcp-oauth/callback?state={}&code=good", q["state"]), &[]).await;
    assert_eq!(s, 400, "the state works once");

    let (_, r) = t.call("GET", "/api/mcp-oauth", Some(&session), None).await;
    assert_eq!((r["servers"][0]["signed_in"].as_bool(), r["servers"][0]["renewable"].as_bool()), (Some(true), Some(true)));
    assert!(r.to_string().find("a1").is_none(), "the app never sees a token");

    // the token ends within 15 minutes: the server refreshes it before handing it to a machine
    let (_, r) = t.machine("GET", "/api/secrets/values", None).await;
    assert_eq!(r["values"]["NOOP_OAUTH_TRACKER"], "a2");
    let (_, r) = t.machine("GET", "/api/secrets/values", None).await;
    assert_eq!(r["values"]["NOOP_OAUTH_TRACKER"], "a2", "kept: no second refresh");
    assert_eq!(forms.lock().unwrap().len(), 2);

    let (_, r) = t.call("DELETE", "/api/mcp-oauth?name=tracker", Some(&session), None).await;
    assert_eq!(r["servers"], json!([]));
}
