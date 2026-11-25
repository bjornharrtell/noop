//! Shared-file path rules. Port of the path helpers in cloud/src/files.ts.

/// Relative, normalized, no traversal, no git internals.
pub fn valid_path(p: &str) -> bool {
    if p.is_empty() || p.encode_utf16().count() > 512 || p.starts_with('/') || p.contains('\\') || p.contains('\0') {
        return false;
    }
    p.split('/').all(|s| !s.is_empty() && s != "." && s != ".." && s != ".git")
}

/// Prefixes the owner session may read (rules, memory, agent profiles, closed decisions). Never config / secrets.
pub const READABLE: [&str; 5] = ["policies/", "memory/", "agents/", "decisions/closed/", "reports/"];

pub fn readable(p: &str) -> bool {
    let t = p.strip_suffix('/').unwrap_or(p);
    valid_path(if t.is_empty() { "x" } else { t }) && READABLE.iter().any(|r| p.starts_with(r))
}

/// Exclusive upper bound for a prefix range scan on the primary key: "a/" → "a0".
pub fn prefix_end(p: &str) -> String {
    let mut chars: Vec<char> = p.chars().collect();
    if let Some(last) = chars.pop() {
        chars.push(char::from_u32(last as u32 + 1).unwrap_or(char::MAX));
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths() {
        assert!(valid_path("tasks/inbox/T-1.md"));
        for bad in ["", "/a", "a//b", "../etc/passwd", "a/./b", ".git/config", "a\\b", "a\0"] {
            assert!(!valid_path(bad), "{bad:?}");
        }
        assert!(readable("memory/"));
        assert!(readable("memory/x.md"));
        assert!(!readable("config.toml"));
        assert_eq!(prefix_end("memory/"), "memory0");
    }
}
