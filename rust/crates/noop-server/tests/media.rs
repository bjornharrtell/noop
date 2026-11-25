//! Media and the owner's uploads.
mod common;

use common::*;
use serde_json::json;

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4];

#[tokio::test]
async fn owner_upload_then_message() {
    let t = T::new();
    let (session, _) = t.sign_in().await;
    // a phone photo sent as application/octet-stream is taken as what its bytes say
    let (s, r) = t.upload("/api/uploads", &session, Some("IMG 1.png"), Some("application/octet-stream"), PNG).await;
    assert_eq!(s, 200, "{r}");
    let url = r["url"].as_str().unwrap().to_string();
    assert!(url.starts_with("/media/") && url.ends_with("-IMG_1.png"), "{url}");

    let (s, ev) = t.call("POST", "/api/messages", Some(&session), Some(json!({ "text": "", "attachments": [url] }))).await;
    assert_eq!((s, ev["attachments"][0].as_str()), (200, Some(url.as_str())));

    // served inline, sandboxed; ?token= works for <img>
    let (s, body, h) = t.bytes("GET", &format!("{url}?token={session}"), &[]).await;
    assert_eq!((s, body.as_slice()), (200, PNG));
    assert_eq!(h["content-type"], "image/png");
    assert!(h["content-disposition"].starts_with("inline"));
    assert!(h["content-security-policy"].starts_with("sandbox"));
    let (s, _, _) = t.bytes("GET", &url, &[]).await;
    assert_eq!(s, 401, "media needs a credential");

    // refused: SVG (can carry script), empty files, plain text fields
    let (s, r) = t.upload("/api/uploads", &session, Some("x.svg"), Some("image/svg+xml"), b"<svg/>").await;
    assert_eq!((s, r["error"].as_str()), (415, Some("images (png, jpeg, gif, webp, heic, bmp) or Markdown only")));
    let (s, _) = t.upload("/api/uploads", &session, Some("e.png"), Some("image/png"), b"").await;
    assert_eq!(s, 400);
    let (s, r) = t.upload("/api/uploads", &session, None, None, b"text").await;
    assert_eq!((s, r["error"].as_str()), (400, Some("file required")));
    let (s, _) = t.upload("/api/uploads", &session, Some("big.png"), Some("image/png"), &[PNG, &vec![0u8; 10 * 1024 * 1024]].concat()).await;
    assert_eq!(s, 413);
}

#[tokio::test]
async fn machine_media_and_ranges() {
    let t = T::new();
    let data: Vec<u8> = (0..2_500_000u32).map(|i| (i % 251) as u8).collect(); // three 1 MB chunks
    let (s, r) = t.upload("/api/media", TOKEN, Some("rec.mp4"), Some("video/mp4"), &data).await;
    assert_eq!(s, 200, "{r}");
    let url = r["url"].as_str().unwrap().to_string();
    let path = format!("{url}?token={TOKEN}");

    let (s, body, h) = t.bytes("GET", &path, &[]).await;
    assert_eq!((s, body.len()), (200, data.len()));
    assert_eq!(body, data);
    assert_eq!((h["accept-ranges"].as_str(), h["content-type"].as_str()), ("bytes", "video/mp4"));

    let (s, body, h) = t.bytes("GET", &path, &[("range", "bytes=999990-1000009")]).await;
    assert_eq!(s, 206);
    assert_eq!(body, &data[999_990..=1_000_009], "a range across a chunk boundary");
    assert_eq!(h["content-range"], format!("bytes 999990-1000009/{}", data.len()));
    let (_, body, _) = t.bytes("GET", &path, &[("range", "bytes=-10")]).await;
    assert_eq!(body, &data[data.len() - 10..], "suffix range");
    let (s, _, h) = t.bytes("GET", &path, &[("range", "bytes=9999999-")]).await;
    assert_eq!((s, h["content-range"].clone()), (416, format!("bytes */{}", data.len())));
    let (s, body, h) = t.bytes("HEAD", &path, &[]).await;
    assert_eq!((s, body.len(), h["content-length"].clone()), (200, 0, data.len().to_string()));

    // a page uploaded as media is a download, never rendered on the app's origin
    let (_, r) = t.upload("/api/media", TOKEN, Some("x.html"), Some("text/html"), b"<script>").await;
    let (_, _, h) = t.bytes("GET", &format!("{}?token={TOKEN}", r["url"].as_str().unwrap()), &[]).await;
    assert_eq!(h["content-type"], "application/octet-stream");
    assert!(h["content-disposition"].starts_with("attachment"));

    let (s, _, _) = t.bytes("GET", &format!("/media/nope?token={TOKEN}"), &[]).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn old_media_is_pruned() {
    let t = T::new();
    let (_, r) = t.upload("/api/media", TOKEN, Some("old.png"), Some("image/png"), PNG).await;
    let old = format!("{}?token={TOKEN}", r["url"].as_str().unwrap());
    // default is 7 days; pruning runs at most hourly, on the next event
    t.advance(8 * 86_400_000);
    t.machine("POST", "/api/events", Some(json!({ "from": "dev", "kind": "notice", "text": "tick" }))).await;
    let (s, _, _) = t.bytes("GET", &old, &[]).await;
    assert_eq!(s, 404);
}
