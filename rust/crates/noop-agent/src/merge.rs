//! Merging two versions of an append-only file.
//!
//! Journals and logs are only ever added to, so two machines appending on the same day is the normal case, not a
//! conflict. The merge is a line union: the version already on the server, then this machine's lines it does not have.
//! Repeated lines are counted, so a line appended twice on one machine is not silently collapsed.

use std::collections::HashMap;

/// Paths that only ever grow. Ported from the TypeScript client's `appendOnly`.
pub fn append_only(rel: &str) -> bool {
    rel.starts_with("journal/")
        || rel.starts_with("logs/")
        || rel == "memory/lessons.md"
        || rel == "memory/judge-feedback.md"
}

/// A conflict marker left by a merge tool: dropped, so a merge never produces more of them.
fn marker(line: &str) -> bool {
    let b = line.as_bytes();
    let runs = *b"<=>";
    runs.iter().any(|c| line.len() >= 7 && b[..7].iter().all(|x| x == c) && (b.len() == 7 || b[7] == b' '))
}

/// The remote version, then this machine's lines the remote does not already have (counting repeats).
pub fn union_lines(local: &str, remote: &str) -> String {
    let mut have: HashMap<&str, i64> = HashMap::new();
    let mut out: Vec<String> = Vec::new();
    for line in remote.split('\n').filter(|l| !marker(l)) {
        if line.is_empty() {
            continue;
        }
        *have.entry(line).or_insert(0) += 1;
        out.push(line.to_string());
    }
    for line in local.split('\n').filter(|l| !marker(l)) {
        if line.is_empty() {
            continue;
        }
        match have.get_mut(line) {
            Some(n) if *n > 0 => *n -= 1,
            _ => out.push(line.to_string()),
        }
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// Merge only when it is safe: two texts, and a path that only grows.
pub fn merge(rel: &str, local: &[u8], remote: &[u8]) -> Option<Vec<u8>> {
    if !append_only(rel) {
        return None;
    }
    let l = std::str::from_utf8(local).ok()?;
    let r = std::str::from_utf8(remote).ok()?;
    Some(union_lines(l, r).into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_growing_paths_merge() {
        for p in ["journal/2026-11-26.md", "logs/run.md", "memory/lessons.md", "memory/judge-feedback.md"] {
            assert!(append_only(p), "{p}");
        }
        for p in ["journal", "memory/focus.md", "tasks/inbox/T-1.md", "notes/journal/x.md"] {
            assert!(!append_only(p), "{p}");
        }
    }

    #[test]
    fn both_sides_lines_survive() {
        let remote = "# Journal 2026-11-26\n\n- 09:00 `a` first\n";
        let local = "# Journal 2026-11-26\n\n- 09:05 `b` second\n";
        let m = union_lines(local, remote);
        assert!(m.contains("first"), "{m}");
        assert!(m.contains("second"), "{m}");
        assert_eq!(m.lines().next(), Some("# Journal 2026-11-26"), "the header appears once: {m}");
    }

    #[test]
    fn a_line_written_twice_stays_twice() {
        let remote = "a\nb\n";
        let local = "a\nb\nb\n";
        assert_eq!(union_lines(local, remote), "a\nb\nb\n");
    }

    #[test]
    fn conflict_markers_are_dropped() {
        let remote = "a\n<<<<<<< HEAD\n";
        let local = "b\n>>>>>>> theirs\n";
        let m = union_lines(local, remote);
        assert!(!m.contains("<<<<<<<") && !m.contains(">>>>>>>"), "{m}");
        assert!(m.contains('a') && m.contains('b'), "{m}");
    }

    #[test]
    fn merge_refuses_other_paths_and_non_text() {
        assert!(merge("journal/x.md", b"a\n", b"b\n").is_some());
        assert!(merge("memory/focus.md", b"a\n", b"b\n").is_none());
        assert!(merge("journal/x.md", &[0xff], b"b\n").is_none());
    }
}
