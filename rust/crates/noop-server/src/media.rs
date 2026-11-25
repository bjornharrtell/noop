//! Screenshots, recordings and the owner's images. Port of cloud/src/uploads.ts and the media routes in index.ts:
//!
//!   POST /api/media    (multipart `file`, ≤ 30 MB)            machines' screenshots and recordings → {url}
//!   POST /api/uploads  (multipart `file`, images/Markdown, ≤ 10 MB) the owner's attachments → {url}
//!   GET  /media/<id>   (Bearer or ?token=)                    the file, with single byte ranges for players
//!
//! Stored in the `media` table in 1 MB chunks, as the Worker did, so exports stay compatible.
use std::sync::LazyLock;

use axum::body::Body;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use regex::Regex;
use rusqlite::params;
use serde_json::json;

use crate::app::{err, ok, App, Req};

const CHUNK: usize = 1_000_000;
const MAX_MEDIA: usize = 30_000_000;
pub const MAX_UPLOAD: usize = 10 * 1024 * 1024;

static UPLOAD_TYPES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(image/(png|jpeg|gif|webp|heic|heif|avif|bmp)|text/markdown)$").unwrap());
static INLINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(image/(png|jpe?g|gif|webp|avif|heic|heif|bmp)|video/[\w.+-]+|audio/[\w.+-]+|application/pdf|text/plain)(;|$)").unwrap()
});
static RANGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^bytes=(\d*)-(\d*)$").unwrap());

/// `name.replace(/[^\w.\-]/g, "_")` (ASCII \w, as in JS).
fn safe_name(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' { c } else { '_' }).collect()
}

/// The image's real type from its first bytes. Phone clients often send no type, "image/jpg" or
/// "application/octet-stream" for photos; trusting the bytes is both more lenient and safer.
pub fn sniff_image(head: &[u8], declared: &str) -> String {
    let ascii = |i: usize, n: usize| head.get(i..i + n).map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
    if head.first() == Some(&0x89) && ascii(1, 3) == "PNG" {
        return "image/png".into();
    }
    if head.starts_with(&[0xff, 0xd8, 0xff]) {
        return "image/jpeg".into();
    }
    if ascii(0, 4) == "GIF8" {
        return "image/gif".into();
    }
    if ascii(0, 4) == "RIFF" && ascii(8, 4) == "WEBP" {
        return "image/webp".into();
    }
    if ascii(0, 2) == "BM" {
        return "image/bmp".into();
    }
    if ascii(4, 4) == "ftyp" {
        let brand = ascii(8, 4);
        if ["heic", "heix", "hevc", "heim", "heis", "mif1", "msf1"].contains(&brand.as_str()) {
            return if brand.starts_with("mif") || brand.starts_with("msf") { "image/heif" } else { "image/heic" }.into();
        }
        if brand == "avif" || brand == "avis" {
            return "image/avif".into();
        }
    }
    if declared == "image/jpg" { "image/jpeg".into() } else { declared.into() }
}

/// Why the owner's upload is refused (None: it is taken). SVG is refused: it can carry script.
pub fn upload_problem(ty: &str, size: usize) -> Option<(&'static str, u16)> {
    if !UPLOAD_TYPES.is_match(ty) {
        return Some(("images (png, jpeg, gif, webp, heic, bmp) or Markdown only", 415));
    }
    if size > MAX_UPLOAD {
        return Some(("file too large (10 MB max)", 413));
    }
    if size == 0 {
        return Some(("empty file", 400));
    }
    None
}

/// A stored upload's id: unique, and safe as a file name and a URL segment.
pub fn upload_id(now: i64, name: &str) -> String {
    let safe = safe_name(if name.is_empty() { "image" } else { name });
    let tail: String = safe.chars().rev().take(80).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{now}-{}-{tail}", &uuid::Uuid::new_v4().to_string()[..8])
}

/// Media is served from the app's own origin: anything a browser could run as a page is sent as a download, never
/// rendered, and every response is sandboxed — an uploaded file must not be able to read the session.
pub fn media_headers(ty: &str, name: &str) -> Vec<(&'static str, String)> {
    let t = if ty.is_empty() { "application/octet-stream".to_string() } else { ty.to_lowercase() };
    let inline = INLINE.is_match(&t);
    vec![
        ("content-type", if inline { t } else { "application/octet-stream".into() }),
        ("content-disposition", format!("{}; filename=\"{}\"", if inline { "inline" } else { "attachment" }, safe_name(name))),
        ("x-content-type-options", "nosniff".into()),
        ("content-security-policy", "sandbox; default-src 'none'; img-src 'self' data:; media-src 'self'; style-src 'unsafe-inline'".into()),
        ("cache-control", "private, max-age=31536000".into()),
    ]
}

/// Native players probe the file and seek with single byte-range requests.
pub fn media_response(req: &Req, data: Vec<u8>, ty: &str, name: &str) -> Response {
    let mut headers = media_headers(ty, name);
    let size = data.len();
    headers.push(("accept-ranges", "bytes".into()));
    let build = |status: StatusCode, headers: &[(&'static str, String)], body: Body| {
        let mut b = Response::builder().status(status);
        for (k, v) in headers {
            b = b.header(*k, v);
        }
        b.body(body).expect("valid response")
    };
    let range = req.header("range").and_then(|r| RANGE.captures(r));
    let full = match &range {
        None => true,
        Some(c) => (c[1].is_empty() && c[2].is_empty()) || req.header("if-range").is_some(),
    };
    if req.method == Method::HEAD || full {
        headers.push(("content-length", size.to_string()));
        let body = if req.method == Method::HEAD { Body::empty() } else { Body::from(data) };
        return build(StatusCode::OK, &headers, body);
    }
    let c = range.expect("checked above");
    let num = |s: &str| s.parse::<u128>().unwrap_or(u128::MAX);
    let size_u = size as u128;
    let start = if !c[1].is_empty() { num(&c[1]) } else { size_u.saturating_sub(num(&c[2])) };
    let end = if !c[1].is_empty() && !c[2].is_empty() { num(&c[2]).min(size_u.saturating_sub(1)) } else { size_u.saturating_sub(1) };
    if size == 0 || start > end {
        headers.push(("content-range", format!("bytes */{size}")));
        headers.push(("content-length", "0".into()));
        return build(StatusCode::RANGE_NOT_SATISFIABLE, &headers, Body::empty());
    }
    let (s, e) = (start as usize, end as usize);
    headers.push(("content-range", format!("bytes {s}-{e}/{size}")));
    headers.push(("content-length", (e - s + 1).to_string()));
    build(StatusCode::PARTIAL_CONTENT, &headers, Body::from(data[s..=e].to_vec()))
}

/// The multipart field `file`: (file name, declared type, bytes). `is_file` is false for a plain text field.
pub struct Part {
    pub name: String,
    pub ty: String,
    pub data: Vec<u8>,
    pub is_file: bool,
}

pub async fn file_field(req: &Req) -> Option<Part> {
    let boundary = multer::parse_boundary(req.header("content-type")?).ok()?;
    let body = req.body.clone();
    let mut mp = multer::Multipart::new(futures_util::stream::once(async move { Ok::<_, std::io::Error>(body) }), boundary);
    while let Ok(Some(field)) = mp.next_field().await {
        if field.name() != Some("file") {
            continue;
        }
        let is_file = field.file_name().is_some();
        let name = field.file_name().unwrap_or("blob").to_string();
        let ty = field.content_type().map(|m| m.to_string()).unwrap_or_default();
        let data = field.bytes().await.ok()?.to_vec();
        return Some(Part { name, ty, data, is_file });
    }
    None
}

fn store(app: &App, id: &str, ty: &str, data: &[u8]) -> rusqlite::Result<()> {
    let mut c = app.db();
    let tx = c.transaction()?;
    for (n, chunk) in data.chunks(CHUNK).enumerate() {
        tx.execute("INSERT INTO media (id, n, type, data) VALUES (?, ?, ?, ?)", params![id, n as i64, ty, chunk])?;
    }
    if data.is_empty() {
        tx.execute("INSERT INTO media (id, n, type, data) VALUES (?, 0, ?, ?)", params![id, ty, Vec::<u8>::new()])?;
    }
    tx.commit()
}

/// `decodeURIComponent` ('+' stays '+').
fn decode_component(s: &str) -> String {
    crate::app::pct_decode(&s.replace('+', "%2B"))
}

/// The media routes; None when the path is not one of them.
pub async fn routes(app: &App, req: &Req) -> rusqlite::Result<Option<Response>> {
    let p = req.path.as_str();
    if p == "/api/uploads" && req.method == Method::POST {
        let Some(f) = file_field(req).await.filter(|f| f.is_file) else { return Ok(Some(err("file required", 400))) };
        let head = &f.data[..f.data.len().min(16)];
        let ty = sniff_image(head, &f.ty);
        if let Some((msg, status)) = upload_problem(&ty, f.data.len()) {
            return Ok(Some(err(msg, status)));
        }
        let id = upload_id(app.now(), &f.name);
        store(app, &id, &ty, &f.data)?;
        return Ok(Some(ok(json!({ "url": format!("/media/{id}") }))));
    }
    if p == "/api/media" && req.method == Method::POST {
        let Some(f) = file_field(req).await else { return Ok(Some(err("file required", 400))) };
        if f.data.len() > MAX_MEDIA {
            return Ok(Some(err("file too large", 413)));
        }
        let id = format!("{}-{}", app.now(), safe_name(&f.name));
        let ty = if f.ty.is_empty() { "application/octet-stream".to_string() } else { f.ty };
        store(app, &id, &ty, &f.data)?;
        return Ok(Some(ok(json!({ "url": format!("/media/{id}") }))));
    }
    if let Some(raw) = p.strip_prefix("/media/") {
        let id = decode_component(raw);
        let (ty, data) = {
            let c = app.db();
            let mut st = c.prepare_cached("SELECT type, data FROM media WHERE id = ? ORDER BY n")?;
            let rows = st
                .query_map([&id], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<Vec<u8>>>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if rows.is_empty() {
                return Ok(Some(err("not found", 404)));
            }
            let ty = rows[0].0.clone().unwrap_or_default();
            (ty, rows.into_iter().flat_map(|r| r.1.unwrap_or_default()).collect::<Vec<u8>>())
        };
        return Ok(Some(media_response(req, data, &ty, &id)));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sniffing() {
        assert_eq!(sniff_image(&[0x89, b'P', b'N', b'G'], ""), "image/png");
        assert_eq!(sniff_image(&[0xff, 0xd8, 0xff, 0xe0], "application/octet-stream"), "image/jpeg");
        assert_eq!(sniff_image(b"....ftypheic", ""), "image/heic");
        assert_eq!(sniff_image(b"....ftypmif1", ""), "image/heif");
        assert_eq!(sniff_image(b"<svg", "image/svg+xml"), "image/svg+xml");
        assert_eq!(sniff_image(b"", "image/jpg"), "image/jpeg");
        assert!(upload_problem("image/svg+xml", 10).is_some());
        assert!(upload_problem("text/markdown", 10).is_none());
        assert_eq!(upload_problem("image/png", 0), Some(("empty file", 400)));
    }
    #[test]
    fn ids_and_headers() {
        let id = upload_id(1, "my photo (1).PNG");
        assert!(id.starts_with("1-") && id.ends_with("-my_photo__1_.PNG"), "{id}");
        let h = media_headers("text/html", "x.html");
        assert_eq!(h[0].1, "application/octet-stream");
        assert!(h[1].1.starts_with("attachment"));
        assert!(media_headers("image/png", "a.png")[1].1.starts_with("inline"));
    }
}
