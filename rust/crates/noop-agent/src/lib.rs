//! The machine program's sync core: a local mirror of the fleet's shared files, kept in step with a Noop server.
//!
//! Ported from the TypeScript client (src/sync.ts: pull, scan, commit). The mirror is a plain directory tree, so other
//! programs (and the agents it starts) read and write ordinary files. Not ported yet: append-only merging of two edits to
//! one file, damaged-file detection, the sync-base snapshot used for recovery, and the cross-process lock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Bigger files are skipped: the server refuses them too, and a mirror is not a place for datasets.
pub const MAX_FILE: usize = 1_500_000;

/// Model a run uses unless `[agent] model` says otherwise. Copilot names its own models; if this one is not on the
/// account's plan the run fails with the runtime's message and `[agent] model` picks another.
pub const DEFAULT_MODEL: &str = "gpt-6-luna";

pub mod gates;

pub mod work;

#[cfg(feature = "copilot")]
pub mod copilot;

/// Where a machine's mirror lives and how it reaches its server.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub url: String,
    pub token: String,
    /// Holds `data/` (the mirror), `sync.json` (what was last seen) and `noop.toml` (the bootstrap).
    pub home: PathBuf,
}

impl Ctx {
    pub fn new(url: impl Into<String>, token: impl Into<String>, home: impl Into<PathBuf>) -> Self {
        let mut url = url.into();
        while url.ends_with('/') {
            url.pop();
        }
        Self { url, token: token.into(), home: home.into() }
    }

    /// The mirrored files themselves.
    pub fn mirror(&self) -> PathBuf {
        self.home.join("data")
    }

    fn state_path(&self) -> PathBuf {
        self.home.join("sync.json")
    }

    /// The bootstrap file (`HUB_CONF` overrides), as the TypeScript client names it.
    pub fn config_path(&self) -> PathBuf {
        std::env::var("HUB_CONF").map(PathBuf::from).unwrap_or_else(|_| self.home.join("noop.toml"))
    }

    /// One request to the server, with the machine key.
    pub async fn call(&self, method: &str, path: &str, body: Option<serde_json::Value>) -> Result<(u16, serde_json::Value), String> {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().map_err(|e| e.to_string())?;
        let mut req = client
            .request(method.parse().map_err(|_| format!("bad method {method}"))?, format!("{}{path}", self.url))
            .bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let r = req.send().await.map_err(|e| format!("{method} {path}: {e}"))?;
        let code = r.status().as_u16();
        let text = r.text().await.map_err(|e| e.to_string())?;
        let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        Ok((code, json))
    }
}

fn num(v: Option<&serde_json::Value>) -> i64 {
    v.and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))).unwrap_or(0)
}

fn str_of(v: Option<&serde_json::Value>) -> &str {
    v.and_then(|v| v.as_str()).unwrap_or("")
}

fn sha256hex(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    format!("{:x}", h.finalize())
}

// ------------------------------------------------------------------ what was last seen

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub version: i64,
    pub hash: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct State {
    /// Server revision the mirror was last in step with.
    #[serde(default)]
    pub since: i64,
    #[serde(default)]
    pub files: BTreeMap<String, FileState>,
}

fn load_state(ctx: &Ctx) -> State {
    std::fs::read_to_string(ctx.state_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_state(ctx: &Ctx, st: &State) -> Result<(), String> {
    std::fs::create_dir_all(&ctx.home).map_err(|e| format!("{}: {e}", ctx.home.display()))?;
    let json = serde_json::to_vec(st).map_err(|e| e.to_string())?;
    write_atomic(&ctx.state_path(), &json)
}

/// Write through a temporary name so a crash never leaves a truncated file under the real name.
fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, data).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

// ------------------------------------------------------------------ the mirror

/// Paths never mirrored: git internals, logs, machine-local config, editor and temp files.
pub fn ignored(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or("");
    let name = rel.rsplit('/').next().unwrap_or("");
    if first == ".git" || first == "logs" || rel == "config.local.toml" {
        return true;
    }
    rel.split('/').any(|p| p == ".git")
        || name.ends_with(".tmp")
        || name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swo")
        || name == ".DS_Store"
        || (name.starts_with('#') && name.ends_with('#'))
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        if ignored(&r) {
            continue;
        }
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&e.path(), &r, out),
            Ok(t) if t.is_file() => out.push(r),
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Change {
    pub path: String,
    pub base_version: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
    #[serde(skip)]
    pub hash: Option<String>,
}

/// Local changes against the recorded state: new content, new files, and files that are gone.
pub fn scan(ctx: &Ctx, st: &State) -> Vec<Change> {
    let mut out = Vec::new();
    let mut files = Vec::new();
    walk(&ctx.mirror(), "", &mut files);
    for rel in &files {
        if !noop_core::paths::valid_path(rel) {
            continue;
        }
        let Ok(bytes) = std::fs::read(ctx.mirror().join(rel)) else { continue };
        if bytes.len() > MAX_FILE {
            eprintln!("sync: skipping {rel} ({} bytes > {MAX_FILE})", bytes.len());
            continue;
        }
        let hash = sha256hex(&bytes);
        let known = st.files.get(rel);
        if known.is_some_and(|k| k.hash == hash) {
            continue;
        }
        out.push(Change {
            path: rel.clone(),
            base_version: known.map(|k| k.version).unwrap_or(0),
            content: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
            deleted: None,
            hash: Some(hash),
        });
    }
    for (rel, k) in &st.files {
        let gone = !ctx.mirror().join(rel).is_file();
        if gone {
            out.push(Change { path: rel.clone(), base_version: k.version, content: None, deleted: Some(true), hash: None });
        }
    }
    out
}

// ------------------------------------------------------------------ pull

#[derive(Debug, Default, PartialEq)]
pub struct Pull {
    pub written: usize,
    pub deleted: usize,
    /// Local edits that a remote version would have overwritten, kept in place for the next commit.
    pub kept: usize,
    /// Server revision reached.
    pub rev: i64,
}

/// Bring the mirror up to date with the server.
///
/// A file this machine has changed but not yet sent is never overwritten: the remote version is recorded (so the next
/// commit is based on it) but the local bytes stay, and the file counts as pending. Without that, a pull between an edit
/// and its push would drop the edit silently — the case task claiming depends on noticing.
pub async fn pull(ctx: &Ctx) -> Result<Pull, String> {
    let mut st = load_state(ctx);
    let mut out = Pull::default();
    // what this machine changed and has not sent yet, before anything from the server lands
    let unsent: Vec<(String, Vec<u8>)> = scan(ctx, &st)
        .into_iter()
        .filter(|c| c.deleted.is_none())
        .filter_map(|c| std::fs::read(ctx.mirror().join(&c.path)).ok().map(|b| (c.path, b)))
        .collect();
    loop {
        let (code, body) = ctx.call("GET", &format!("/api/files/changes?since={}", st.since), None).await?;
        if code != 200 {
            return Err(format!("pull: server answered {code} {}", str_of(body.get("error"))));
        }
        let more = body.get("more").and_then(|v| v.as_bool()).unwrap_or(false);
        for f in body.get("files").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            let path = str_of(f.get("path")).to_string();
            if !noop_core::paths::valid_path(&path) {
                continue;
            }
            if f.get("deleted").and_then(|v| v.as_bool()).unwrap_or(false) {
                if std::fs::remove_file(ctx.mirror().join(&path)).is_ok() {
                    out.deleted += 1;
                }
                st.files.remove(&path);
                continue;
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(str_of(f.get("content")))
                .map_err(|e| format!("pull {path}: {e}"))?;
            write_atomic(&ctx.mirror().join(&path), &bytes)?;
            st.files.insert(path, FileState { version: num(f.get("version")), hash: str_of(f.get("hash")).to_string() });
            out.written += 1;
        }
        out.rev = num(body.get("rev"));
        st.since = out.rev;
        if !more {
            break;
        }
    }
    // put back the local edits the remote versions just displaced; they are sent by the next commit
    for (path, bytes) in unsent {
        if std::fs::read(ctx.mirror().join(&path)).ok().as_deref() != Some(&bytes[..]) {
            write_atomic(&ctx.mirror().join(&path), &bytes)?;
            out.kept += 1;
        }
    }
    save_state(ctx, &st)?;
    Ok(out)
}

// ------------------------------------------------------------------ commit

#[derive(Debug, Default, PartialEq)]
pub struct Commit {
    pub pushed: usize,
    pub rev: i64,
}

/// Send local changes. A rejection means another machine wrote first: pull, keep this machine's edit, and retry.
pub async fn commit(ctx: &Ctx, msg: &str) -> Result<Commit, String> {
    for attempt in 1..=3 {
        let mut st = load_state(ctx);
        let changes = scan(ctx, &st);
        if changes.is_empty() {
            return Ok(Commit { pushed: 0, rev: st.since });
        }
        let pushed = changes.len();
        let (code, body) = ctx.call("POST", "/api/files/commit", Some(serde_json::json!({ "msg": msg, "changes": changes }))).await?;
        match code {
            200 => {
                let rev = num(body.get("rev"));
                for f in body.get("files").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                    let path = str_of(f.get("path")).to_string();
                    if f.get("deleted").and_then(|v| v.as_bool()).unwrap_or(false) {
                        st.files.remove(&path);
                    } else {
                        st.files.insert(path, FileState { version: num(f.get("version")), hash: str_of(f.get("hash")).to_string() });
                    }
                }
                st.since = rev;
                save_state(ctx, &st)?;
                return Ok(Commit { pushed, rev });
            }
            409 => {
                if attempt == 3 {
                    break;
                }
                pull(ctx).await?;
            }
            _ => return Err(format!("commit: server answered {code} {}", str_of(body.get("error")))),
        }
    }
    Err("commit: still conflicting after 3 attempts".into())
}

/// Send local changes once, without retrying: `Ok(None)` means another machine committed first.
///
/// Task claiming uses this. The retrying [`commit`] pulls and re-sends, which for a move would claim the same task twice;
/// a claim must lose cleanly instead.
pub async fn commit_once(ctx: &Ctx, msg: &str) -> Result<Option<Commit>, String> {
    let mut st = load_state(ctx);
    let changes = scan(ctx, &st);
    if changes.is_empty() {
        return Ok(Some(Commit { pushed: 0, rev: st.since }));
    }
    let pushed = changes.len();
    let (code, body) = ctx.call("POST", "/api/files/commit", Some(serde_json::json!({ "msg": msg, "changes": changes }))).await?;
    match code {
        200 => {
            let rev = num(body.get("rev"));
            for f in body.get("files").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                let path = str_of(f.get("path")).to_string();
                if f.get("deleted").and_then(|v| v.as_bool()).unwrap_or(false) {
                    st.files.remove(&path);
                } else {
                    st.files.insert(path, FileState { version: num(f.get("version")), hash: str_of(f.get("hash")).to_string() });
                }
            }
            st.since = rev;
            save_state(ctx, &st)?;
            Ok(Some(Commit { pushed, rev }))
        }
        409 => Ok(None),
        _ => Err(format!("commit: server answered {code} {}", str_of(body.get("error")))),
    }
}

// ------------------------------------------------------------------ status

/// A short human summary: is the server answering, what the mirror holds, what is unsent.
pub async fn status(ctx: &Ctx) -> Result<String, String> {
    let (code, body) = ctx.call("GET", "/api/status", None).await?;
    if code != 200 {
        return Err(format!("server {}: {code} {}", ctx.url, str_of(body.get("error"))));
    }
    let st = load_state(ctx);
    let pending = scan(ctx, &st).len();
    let mut lines = vec![
        format!("server: {} (ok)", ctx.url),
        format!("mirror: {} ({} files tracked, rev {})", ctx.mirror().display(), st.files.len(), st.since),
    ];
    lines.push(if pending == 0 { "pending: nothing to push".into() } else { format!("pending: {pending} change(s) to push") });
    Ok(lines.join("\n"))
}

// ------------------------------------------------------------------ bootstrap

#[derive(Debug, Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    cloud: Option<CloudSection>,
    #[serde(default)]
    server: Option<ServerSection>,
    #[serde(default)]
    agent: Option<AgentSection>,
}

#[derive(Debug, Deserialize)]
struct AgentSection {
    #[serde(default)]
    model: Option<String>,
}

/// The model runs use: `[agent] model` in `noop.toml`, else [`DEFAULT_MODEL`].
pub fn read_model(home: &Path) -> String {
    let p = std::env::var("HUB_CONF").map(PathBuf::from).unwrap_or_else(|_| home.join("noop.toml"));
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| toml::from_str::<ConfigFile>(&t).ok())
        .and_then(|c| c.agent)
        .and_then(|a| a.model)
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

#[derive(Debug, Deserialize)]
struct CloudSection {
    url: String,
}

#[derive(Debug, Deserialize)]
struct ServerSection {
    token: String,
}

/// Read `noop.toml` (or `HUB_CONF`).
pub fn read_config(home: &Path) -> Result<(String, String), String> {
    let p = std::env::var("HUB_CONF").map(PathBuf::from).unwrap_or_else(|_| home.join("noop.toml"));
    let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e} (run: noop init <server url> --token <key>)", p.display()))?;
    let c: ConfigFile = toml::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))?;
    let url = c.cloud.map(|c| c.url).filter(|u| !u.trim().is_empty()).ok_or("[cloud] url is missing")?;
    let token = c.server.map(|s| s.token).filter(|t| !t.trim().is_empty()).ok_or("[server] token is missing")?;
    Ok((url, token))
}

/// Write `noop.toml`, readable only by its owner: it holds the machine key.
pub fn write_config(home: &Path, url: &str, token: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(home).map_err(|e| format!("{}: {e}", home.display()))?;
    let p = home.join("noop.toml");
    let text = format!("# Noop machine bootstrap: where this machine's fleet lives. Keeps a secret; mode 600.\n\n[cloud]\nurl = {url:?}\n\n[server]\ntoken = {token:?}\n");
    write_atomic(&p, text.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignored_paths() {
        for p in [".git/config", "logs/a.md", "config.local.toml", "a/b.tmp", "a/.DS_Store", "a/x~", "#a#"] {
            assert!(ignored(p), "{p}");
        }
        for p in ["memory/a.md", "tasks/inbox/T-1.md", "a/b.gitkeep"] {
            assert!(!ignored(p), "{p}");
        }
    }
}
