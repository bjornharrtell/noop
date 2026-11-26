//! What an agent run may not do, whatever the agent decides.
//!
//! The blocks are the product's: merging, releasing, publishing, force-pushing, deleting a repository, reading
//! credentials, and writing outside the run's working directory. Everything else is approved, because a run that asks
//! every few seconds is useless. A later slice lets one owner decision unlock a single class for one run; until then a
//! block is final.

use std::path::Path;

use serde_json::Value;

/// Shell commands whose text contains one of these is refused, `(pattern, reason)`.
const SHELL_BLOCKS: &[(&str, &str)] = &[
    ("gh release", "releases are the owner's call"),
    ("gh pr merge", "merging is the owner's call"),
    ("git merge", "merging is the owner's call"),
    ("npm publish", "publishing is the owner's call"),
    ("cargo publish", "publishing is the owner's call"),
    ("bun publish", "publishing is the owner's call"),
    ("gh repo delete", "deleting a repository is the owner's call"),
    ("git push --delete", "deleting a remote branch is the owner's call"),
    ("rm -rf /", "wiping the filesystem is the owner's call"),
];

/// Paths that carry credentials: refused for reading and for writing.
const SECRET_HINTS: &[&str] = &[".ssh/", "id_rsa", "id_ed25519", ".aws/credentials", ".netrc", "noop.toml", ".env"];

fn norm(cmd: &str) -> String {
    cmd.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Why this shell command is refused, if it is.
pub fn blocked_shell(cmd: &str) -> Option<&'static str> {
    let c = norm(cmd);
    // a force push is a rewrite of published history; a plain push is part of ordinary work
    if c.contains("git push") && (c.contains("--force") || c.contains(" -f ") || c.ends_with(" -f") || c.contains("--mirror")) {
        return Some("force-pushing is the owner's call");
    }
    for (needle, reason) in SHELL_BLOCKS {
        if c.contains(needle) {
            return Some(reason);
        }
    }
    if c.contains("secrets") || SECRET_HINTS.iter().any(|h| c.contains(h)) {
        return Some("credentials stay out of agent runs");
    }
    None
}

/// Why writing (or reading) this path is refused, if it is: it must stay inside the working directory and away from
/// credential files.
pub fn blocked_path(path: &str, cwd: &Path) -> Option<&'static str> {
    let p = path.to_lowercase();
    if SECRET_HINTS.iter().any(|h| p.contains(h)) {
        return Some("credentials stay out of agent runs");
    }
    let candidate = if Path::new(path).is_absolute() { Path::new(path).to_path_buf() } else { cwd.join(path) };
    let normalized = match candidate.canonicalize() {
        Ok(p) => p,
        // the file may not exist yet: check the parent, which must
        Err(_) => {
            let parent = candidate.parent().unwrap_or(&candidate);
            match parent.canonicalize() {
                Ok(p) => p.join(candidate.file_name().unwrap_or_default()),
                Err(_) => candidate.clone(),
            }
        }
    };
    let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    if normalized.starts_with(&root) {
        None
    } else {
        Some("writes stay inside the working directory")
    }
}

/// Every string inside a permission request's arguments, so a rule can be applied without knowing each shape.
fn strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
        Value::Object(o) => o.values().for_each(|x| strings(x, out)),
        _ => {}
    }
}

/// The refusal reason for one permission request, if any.
pub fn blocked_request(extra: &Value, cwd: &Path) -> Option<&'static str> {
    let mut all = Vec::new();
    strings(extra, &mut all);
    for s in &all {
        if let Some(reason) = blocked_shell(s) {
            return Some(reason);
        }
        if s.contains('/') {
            if let Some(reason) = blocked_path(s, cwd) {
                return Some(reason);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shell_blocks() {
        for cmd in [
            "gh release create v1",
            "gh pr merge 12 --squash",
            "git merge main",
            "npm publish",
            "git push --force origin main",
            "git push -f origin main",
            "git push origin main -f",
            "git push --delete origin old",
            "cat ~/.ssh/id_rsa",
            "rm -rf / --no-preserve-root",
        ] {
            assert!(blocked_shell(cmd).is_some(), "{cmd}");
        }
        for cmd in ["git status", "git push origin main", "cargo test", "ls -la", "git commit -m x"] {
            assert!(blocked_shell(cmd).is_none(), "{cmd}");
        }
    }

    #[test]
    fn paths_stay_inside_the_work_dir() {
        let dir = std::env::temp_dir().join(format!("noop-gates-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert!(blocked_path("sub/a.md", &dir).is_none());
        assert!(blocked_path(dir.join("sub/a.md").to_str().unwrap(), &dir).is_none());
        assert!(blocked_path("../outside.md", &dir).is_some());
        assert!(blocked_path("/etc/passwd", &dir).is_some());
        assert!(blocked_path(".env", &dir).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_request_is_read_from_its_arguments() {
        let dir = std::env::temp_dir().join(format!("noop-gates2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let release = json!({ "request": { "command": "gh release create v1.0.0" } });
        assert!(blocked_request(&release, &dir).is_some());
        let fine = json!({ "request": { "command": "cargo test" } });
        assert!(blocked_request(&fine, &dir).is_none());
        let escape = json!({ "request": { "path": "../../etc/passwd" } });
        assert!(blocked_request(&escape, &dir).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }
}
