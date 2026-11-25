//! Web Push without a third-party service: RFC 8291 (aes128gcm message encryption) + RFC 8292 (VAPID, ES256 JWT).
//! Port of cloud/src/push.ts and the push routes in index.ts:
//!
//!   GET  /api/push/vapid        → {publicKey}
//!   POST /api/push/subscribe    {subscription, device_label}   (owner session)
//!   POST /api/push/unsubscribe  {endpoint}                     (owner session)
//!   POST /api/push/test                                        (owner session: this device only)
//!   POST /api/push/answer       {ref, answer}                  (a notification's action button)
//!
//! Keys: HUB_VAPID_PUBLIC (b64url raw P-256 point) and HUB_VAPID_PRIVATE (JWK JSON with `d`), the same values the
//! Worker used, so subscribed browsers keep working. `noop-server vapid-keys` prints a new pair.
use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::Aes128Gcm;
use axum::http::Method;
use axum::response::Response;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToSec1Point;
use p256::{PublicKey, SecretKey};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::Sha256;

use crate::app::{err, ok, App, Req};
use crate::auth::Who;

#[derive(Clone)]
pub struct Vapid {
    /// b64url raw uncompressed point (65 bytes), what browsers take as applicationServerKey.
    pub public_key: String,
    pub signing: SigningKey,
    /// `mailto:` of the first owner.
    pub subject: String,
}

#[derive(Clone, Debug)]
pub struct PushSub {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

pub fn b64u(b: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(b)
}
pub fn unb64u(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

/// The VAPID key pair from HUB_VAPID_PUBLIC / HUB_VAPID_PRIVATE; None when not set up or malformed.
pub fn vapid_from(public: &str, private_jwk: &str, owners: &[String]) -> Option<Vapid> {
    let jwk: Value = serde_json::from_str(private_jwk).ok()?;
    let d = unb64u(jwk.get("d")?.as_str()?)?;
    let secret = SecretKey::from_slice(&d).ok()?;
    let subject = format!("mailto:{}", owners.first().map(String::as_str).unwrap_or("owner@example.com"));
    Some(Vapid { public_key: public.to_string(), signing: SigningKey::from(&secret), subject })
}

/// A new key pair as the two environment values (public b64url point, private JWK JSON).
pub fn new_vapid_keys() -> (String, String) {
    let mut d = [0u8; 32];
    let secret = loop {
        noop_core::random_bytes(&mut d);
        if let Ok(s) = SecretKey::from_slice(&d) {
            break s;
        }
    };
    let point = secret.public_key().to_sec1_point(false);
    let raw = point.as_bytes();
    let jwk = json!({ "kty": "EC", "crv": "P-256", "x": b64u(&raw[1..33]), "y": b64u(&raw[33..65]), "d": b64u(&d) });
    (b64u(raw), jwk.to_string())
}

fn hmac256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// HKDF with a single output block (≤ 32 bytes), as RFC 8291 uses it.
fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let prk = hmac256(salt, ikm);
    let mut i = info.to_vec();
    i.push(1);
    hmac256(&prk, &i)[..len].to_vec()
}

/// RFC 8291 §3–4: encrypt `plaintext` for one subscription. `fixed` = (sender private key, salt), for test vectors.
pub fn encrypt(plaintext: &[u8], sub: &PushSub, fixed: Option<(&[u8], &[u8])>) -> Result<Vec<u8>, String> {
    let ua_public = unb64u(&sub.p256dh).ok_or("bad p256dh")?;
    let auth_secret = unb64u(&sub.auth).ok_or("bad auth")?;
    let ua_key = PublicKey::from_sec1_bytes(&ua_public).map_err(|_| "bad p256dh point")?;
    let as_secret = match fixed {
        Some((d, _)) => SecretKey::from_slice(d).map_err(|_| "bad fixed key")?,
        None => loop {
            let mut d = [0u8; 32];
            noop_core::random_bytes(&mut d);
            if let Ok(s) = SecretKey::from_slice(&d) {
                break s;
            }
        },
    };
    let as_public = as_secret.public_key().to_sec1_point(false).as_bytes().to_vec();
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua_key.as_affine());
    let ecdh = shared.raw_secret_bytes().to_vec();
    let mut info = b"WebPush: info\0".to_vec();
    info.extend_from_slice(&ua_public);
    info.extend_from_slice(&as_public);
    let ikm = hkdf(&auth_secret, &ecdh, &info, 32);
    let salt = match fixed {
        Some((_, s)) => s.to_vec(),
        None => {
            let mut s = vec![0u8; 16];
            noop_core::random_bytes(&mut s);
            s
        }
    };
    let cek = hkdf(&salt, &ikm, b"Content-Encoding: aes128gcm\0", 16);
    let nonce = hkdf(&salt, &ikm, b"Content-Encoding: nonce\0", 12);
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|_| "bad key")?;
    let mut record = plaintext.to_vec();
    record.push(2); // single, last record
    let nonce: [u8; 12] = nonce.try_into().expect("12 bytes");
    let ct = cipher.encrypt(&nonce.into(), record.as_slice()).map_err(|_| "encrypt failed")?;
    let rs: u32 = 4096;
    let mut out = salt;
    out.extend_from_slice(&rs.to_be_bytes());
    out.push(as_public.len() as u8);
    out.extend_from_slice(&as_public);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// RFC 8292: `Authorization: vapid t=<jwt>, k=<public key>` for the push service's origin.
pub fn vapid_auth(endpoint: &str, v: &Vapid, now_ms: i64) -> Result<String, String> {
    let url = reqwest::Url::parse(endpoint).map_err(|e| e.to_string())?;
    let aud = url.origin().ascii_serialization();
    let enc = |o: Value| b64u(o.to_string().as_bytes());
    let unsigned = format!("{}.{}", enc(json!({ "typ": "JWT", "alg": "ES256" })), enc(json!({ "aud": aud, "exp": now_ms / 1000 + 12 * 3600, "sub": v.subject })));
    let sig: Signature = v.signing.sign(unsigned.as_bytes());
    Ok(format!("vapid t={unsigned}.{}, k={}", b64u(&sig.to_bytes()), v.public_key))
}

/// Send one notification; the push service's HTTP status (404/410 → drop the subscription), 0 on a network error.
pub async fn send(http: &reqwest::Client, sub: &PushSub, payload: &Value, v: &Vapid, urgency: &str, topic: Option<&str>, now_ms: i64) -> u16 {
    let Ok(body) = encrypt(payload.to_string().as_bytes(), sub, None) else { return 0 };
    let Ok(auth) = vapid_auth(&sub.endpoint, v, now_ms) else { return 0 };
    let mut r = http
        .post(&sub.endpoint)
        .header("authorization", auth)
        .header("content-encoding", "aes128gcm")
        .header("content-type", "application/octet-stream")
        .header("ttl", "86400")
        .header("urgency", urgency)
        .timeout(std::time::Duration::from_secs(15))
        .body(body);
    if let Some(t) = topic {
        // RFC 8030: ≤ 32 URL-safe characters
        let t: String = t.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(32).collect();
        r = r.header("topic", t);
    }
    match r.send().await {
        Ok(res) => res.status().as_u16(),
        Err(_) => 0,
    }
}

fn subs(c: &Connection, only: Option<&str>) -> rusqlite::Result<Vec<PushSub>> {
    let map = |r: &rusqlite::Row| Ok(PushSub { endpoint: r.get(0)?, p256dh: r.get(1)?, auth: r.get(2)? });
    match only {
        Some(s) => c.prepare_cached("SELECT endpoint, p256dh, auth FROM push_subs WHERE session = ?")?.query_map([s], map)?.collect(),
        None => c.prepare_cached("SELECT endpoint, p256dh, auth FROM push_subs")?.query_map([], map)?.collect(),
    }
}

/// Send to every subscription (or one session's). Expired subscriptions (404/410) are removed.
/// Returns (sent, removed, total).
pub async fn push_all(app: &Arc<App>, list: Vec<PushSub>, payload: Value, urgency: &'static str) -> (usize, usize, usize) {
    let Some(v) = app.vapid.clone() else { return (0, 0, 0) };
    let total = list.len();
    let topic = payload.get("tag").and_then(Value::as_str).map(str::to_string);
    let now = app.now();
    let sends = list.into_iter().map(|sub| {
        let (app, v, payload, topic) = (app.clone(), v.clone(), payload.clone(), topic.clone());
        async move {
            let st = send(&app.http, &sub, &payload, &v, urgency, topic.as_deref(), now).await;
            (sub, st)
        }
    });
    let results = futures_util::future::join_all(sends).await;
    let (mut sent, mut removed) = (0, 0);
    for (sub, st) in results {
        if st == 404 || st == 410 {
            let _ = app.db().execute("DELETE FROM push_subs WHERE endpoint = ?", [&sub.endpoint]);
            removed += 1;
        } else if (200..300).contains(&st) {
            sent += 1;
        } else {
            tracing::error!("push {}: HTTP {st}", reqwest::Url::parse(&sub.endpoint).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default());
        }
    }
    (sent, removed, total)
}

/// After a feed event: the phone notification it deserves, if any (settings notify_prefs), sent in the background.
pub fn notify_event(app: &Arc<App>, c: &Connection, ev: &Value) {
    if app.vapid.is_none() {
        return;
    }
    let settings = crate::db::kv_get(c, "settings").unwrap_or(Value::Null);
    let prefs = settings.pointer("/current/notify_prefs").cloned().unwrap_or_else(|| json!({}));
    let Some((title, body)) = noop_core::notify::notification_for(ev, &prefs, app.now()) else { return };
    let kind = ev.get("kind").and_then(Value::as_str).unwrap_or("").to_string();
    let r = ev.get("ref").and_then(Value::as_str).map(str::to_string);
    let options: Vec<String> = if kind == "decision" || kind == "menu" {
        ev.get("options").and_then(Value::as_array).into_iter().flatten().take(3)
            .map(|o| o.as_str().map(str::to_string).unwrap_or_else(|| o.to_string()).chars().take(20).collect())
            .collect()
    } else {
        Vec::new()
    };
    let status = crate::db::kv_get(c, "status").unwrap_or(Value::Null);
    let badge = status.get("decisions").and_then(Value::as_array).map_or(0, |d| d.iter().filter(|x| x.get("held") != Some(&Value::Bool(true))).count());
    let payload = json!({
        "title": title, "body": body, "ref": r, "kind": kind, "options": options,
        "url": r.as_deref().map(|r| format!("/?ref={}", noop_core::encode_uri_component(r))).unwrap_or_else(|| "/".into()),
        "tag": r.clone().unwrap_or_else(|| kind.clone()), "badge": badge,
    });
    let urgency = if kind == "decision" || kind == "login" { "high" } else { "normal" };
    let Ok(list) = subs(c, None) else { return };
    if list.is_empty() {
        return;
    }
    let app = app.clone();
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(async move {
            push_all(&app, list, payload, urgency).await;
        });
    }
}

fn valid_endpoint(e: &str) -> bool {
    e.starts_with("https://")
        || ["http://127.0.0.1:", "http://127.0.0.1/", "http://localhost:", "http://localhost/"].iter().any(|p| e.starts_with(p))
}

/// /api/push/*; None when the path is not under it.
pub async fn routes(app: &Arc<App>, req: &Req, w: &Who) -> rusqlite::Result<Option<Response>> {
    let p = req.path.as_str();
    if !p.starts_with("/api/push/") {
        return Ok(None);
    }
    if p == "/api/push/vapid" {
        return Ok(Some(match &app.vapid {
            Some(v) => ok(json!({ "publicKey": v.public_key })),
            None => err("push not configured", 503),
        }));
    }
    if req.method != Method::POST {
        return Ok(Some(err("method not allowed", 405)));
    }
    let b = req.json().unwrap_or_else(|| json!({}));
    if p == "/api/push/answer" {
        let r = b.get("ref").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let answer = b.get("answer").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if r.is_empty() || answer.is_empty() {
            return Ok(Some(err("ref and answer required", 400)));
        }
        let c = app.db();
        let text = format!("{r} {answer}");
        let ev = crate::feed::owner_message(app, &c, crate::feed::OwnerMsg { text: &text, reply_to: Some(&r), ..Default::default() })?;
        return Ok(Some(ok(ev)));
    }
    let Who::Session { id: session, .. } = w else { return Ok(Some(err("owner session required", 403))) };
    match p {
        "/api/push/subscribe" => {
            let s = b.get("subscription").cloned().unwrap_or(Value::Null);
            let endpoint = s.get("endpoint").and_then(Value::as_str).unwrap_or("");
            let p256dh = s.pointer("/keys/p256dh").and_then(Value::as_str);
            let auth = s.pointer("/keys/auth").and_then(Value::as_str);
            let (true, Some(p256dh), Some(auth)) = (valid_endpoint(endpoint), p256dh, auth) else {
                return Ok(Some(err("invalid subscription", 400)));
            };
            let label: String = match b.get("device_label") {
                Some(Value::String(l)) => l.chars().take(80).collect(),
                None | Some(Value::Null) => String::new(),
                Some(v) => v.to_string().chars().take(80).collect(),
            };
            app.db().execute(
                "INSERT INTO push_subs (endpoint, session, p256dh, auth, label, created) VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT(endpoint) DO UPDATE SET session = excluded.session, p256dh = excluded.p256dh, auth = excluded.auth, label = excluded.label",
                params![endpoint, session, p256dh, auth, label, app.now()],
            )?;
            Ok(Some(ok(json!({ "ok": true }))))
        }
        "/api/push/unsubscribe" => {
            let endpoint = b.get("endpoint").and_then(Value::as_str).unwrap_or("");
            app.db().execute("DELETE FROM push_subs WHERE endpoint = ? AND session = ?", params![endpoint, session])?;
            Ok(Some(ok(json!({ "ok": true }))))
        }
        "/api/push/test" => {
            if app.vapid.is_none() {
                return Ok(Some(ok(json!({ "ok": true, "sent": 0, "removed": 0, "error": "vapid not configured" }))));
            }
            let list = subs(&app.db(), Some(session))?;
            let payload = json!({ "title": "Noop", "body": app.l("Test notification: phone push is working.", "测试通知：手机推送已开通。"),
                "kind": "notice", "url": "/", "tag": "test", "options": [], "badge": 0 });
            let (sent, removed, total) = push_all(app, list, payload, "normal").await;
            Ok(Some(ok(json!({ "ok": true, "sent": sent, "removed": removed, "total": total }))))
        }
        _ => Ok(Some(err("not found", 404))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::VerifyingKey;

    #[test]
    fn rfc8291_vector() {
        let sub = PushSub {
            endpoint: "https://push.example.net/x".into(),
            p256dh: "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4".into(),
            auth: "BTBZMqHH6r4Tts7J_aSIgg".into(),
        };
        let d = unb64u("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw").unwrap();
        let salt = unb64u("DGv6ra1nlYgDCS1FRnbzlw").unwrap();
        let out = encrypt(b"When I grow up, I want to be a watermelon", &sub, Some((&d, &salt))).unwrap();
        assert_eq!(b64u(&out), "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN");
    }

    #[test]
    fn vapid_jwt_verifies() {
        let (public, private) = new_vapid_keys();
        let v = vapid_from(&public, &private, &["owner@example.com".into()]).unwrap();
        let h = vapid_auth("https://fcm.googleapis.com/fcm/send/abc", &v, 1_780_000_000_000).unwrap();
        let rest = h.strip_prefix("vapid t=").unwrap();
        let (t, k) = rest.split_once(", k=").unwrap();
        assert_eq!(k, public);
        let parts: Vec<&str> = t.split('.').collect();
        let claims: Value = serde_json::from_slice(&unb64u(parts[1]).unwrap()).unwrap();
        assert_eq!((claims["aud"].as_str(), claims["sub"].as_str()), (Some("https://fcm.googleapis.com"), Some("mailto:owner@example.com")));
        let key = VerifyingKey::from_sec1_bytes(&unb64u(&public).unwrap()).unwrap();
        let sig = Signature::from_slice(&unb64u(parts[2]).unwrap()).unwrap();
        key.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();
    }
}
