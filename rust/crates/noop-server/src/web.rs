//! The web client (the NSH web/WebAssembly package from `app/build-web.sh`), served from NOOP_WEB_DIR. Replaces the
//! Worker's static assets binding: unknown paths get index.html (single-page app), and content-hashed files
//! (`nsh.73cc16a15e.wasm`, `app.853c5dd616.js`) are sent as their precompressed `.br` / `.gz` copies when the
//! browser takes them, cached for a year.
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use axum::body::Body;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use regex::Regex;

use crate::app::{err, App, Req};

static HASHED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.[0-9a-f]{10}\.(wasm|m?js|css)$").unwrap());
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "wasm" => "application/wasm",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The file under `root` a URL path names, or None for anything that could leave it.
fn resolve(root: &Path, url_path: &str) -> Option<PathBuf> {
    let p = crate::app::pct_decode(&url_path.replace('+', "%2B"));
    let rel = p.trim_start_matches('/');
    if rel.contains('\\') || rel.contains('\0') || rel.split('/').any(|s| s.starts_with('.') && s != ".well-known") {
        return None;
    }
    Some(root.join(rel))
}

/// Accept-Encoding q-values, e.g. "br;q=1, gzip;q=0.5, *;q=0".
fn quality(accept: &str, name: &str) -> f64 {
    let mut star = None;
    for part in accept.to_lowercase().split(',') {
        let mut it = part.trim().split(';');
        let n = it.next().unwrap_or("").trim();
        let q = it.find_map(|p| p.trim().strip_prefix("q=").map(|v| v.parse::<f64>().unwrap_or(0.0))).unwrap_or(1.0);
        if n == name {
            return q;
        }
        if n == "*" {
            star = Some(q);
        }
    }
    star.unwrap_or(0.0)
}

fn respond(status: StatusCode, headers: &[(&str, &str)], body: Vec<u8>, head: bool) -> Response {
    let mut b = Response::builder().status(status).header("content-length", body.len().to_string());
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(if head { Body::empty() } else { Body::from(body) }).expect("valid response")
}

async fn file(p: &Path) -> Option<Vec<u8>> {
    if !tokio::fs::metadata(p).await.ok()?.is_file() {
        return None;
    }
    tokio::fs::read(p).await.ok()
}

pub async fn serve(app: &App, req: &Req) -> Response {
    if req.method != Method::GET && req.method != Method::HEAD {
        return err("method not allowed", 405);
    }
    let head = req.method == Method::HEAD;
    let Some(root) = app.cfg.web_dir.as_deref() else {
        return err("web client not configured (NOOP_WEB_DIR)", 404);
    };
    let Some(path) = resolve(root, &req.path) else { return err("not found", 404) };

    if HASHED.is_match(&req.path) {
        let ty = content_type(&req.path);
        let accept = req.header("accept-encoding").unwrap_or("");
        let mut variants = [("br", "br"), ("gz", "gzip")];
        variants.sort_by(|a, b| quality(accept, b.1).total_cmp(&quality(accept, a.1)));
        for (ext, enc) in variants {
            if quality(accept, enc) <= 0.0 {
                continue;
            }
            let mut p = path.clone().into_os_string();
            p.push(format!(".{ext}"));
            if let Some(body) = file(Path::new(&p)).await {
                return respond(StatusCode::OK, &[("content-type", ty), ("content-encoding", enc), ("cache-control", IMMUTABLE), ("vary", "accept-encoding")], body, head);
            }
        }
        if let Some(body) = file(&path).await {
            return respond(StatusCode::OK, &[("content-type", ty), ("cache-control", IMMUTABLE), ("vary", "accept-encoding")], body, head);
        }
        return err("not found", 404);
    }

    let target = if req.path.ends_with('/') { path.join("index.html") } else { path };
    if let Some(body) = file(&target).await {
        let name = target.to_string_lossy();
        return respond(StatusCode::OK, &[("content-type", content_type(&name)), ("cache-control", "no-cache")], body, head);
    }
    // single-page app: every other path is the app's own route
    match file(&root.join("index.html")).await {
        Some(body) => respond(StatusCode::OK, &[("content-type", "text/html; charset=utf-8"), ("cache-control", "no-cache")], body, head),
        None => err("not found", 404),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encodings() {
        assert_eq!(quality("gzip, deflate, br", "br"), 1.0);
        assert_eq!(quality("gzip;q=0.5", "br"), 0.0);
        assert_eq!(quality("*;q=0.2", "gzip"), 0.2);
    }
    #[test]
    fn paths() {
        let root = Path::new("/srv/web");
        assert_eq!(resolve(root, "/app.js"), Some(PathBuf::from("/srv/web/app.js")));
        assert_eq!(resolve(root, "/../etc/passwd"), None);
        assert_eq!(resolve(root, "/%2e%2e/etc/passwd"), None);
        assert_eq!(resolve(root, "/.env"), None);
    }
}
