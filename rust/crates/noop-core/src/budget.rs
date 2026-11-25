//! Sync pacing shared by the server and the machine program. Port of the non-Cloudflare parts of cloud/src/budget.ts.
//! The Cloudflare free-plan counters (FREE, TARGET, LEAN, SHED) do not apply to a VPS and are not ported.

/// Journals and heartbeats ride along with real changes, or go up every LAZY_PUSH_MS.
pub const LAZY_PUSH_MS: i64 = 5 * 60_000;
/// A pull right after another one is served from the mirror; commits still detect races through compare-and-swap.
pub const PULL_MIN_MS: i64 = 10_000;
/// Heartbeats go up lazily, so a machine counts as offline only after this long without one.
pub const OFFLINE_AFTER_MINUTES: i64 = 8;
/// An idle guest worker asks for work this often.
pub const GUEST_IDLE_MS: i64 = 5 * 60_000;
/// History is pruned to the latest row of every path once the database passes this size.
pub const STORAGE_TARGET_BYTES: i64 = 500_000_000;

/// Paths rewritten constantly whose old versions are worth nothing: append-only logs and machine heartbeats.
pub fn latest_only(p: &str) -> bool {
    p.starts_with("journal/")
        || p.starts_with("logs/")
        || p.starts_with("fleet/")
        || p == "memory/lessons.md"
        || p == "memory/judge-feedback.md"
}

/// The same rule as SQL, for history pruning.
pub const LATEST_ONLY_SQL: &str = "(path LIKE 'journal/%' OR path LIKE 'logs/%' OR path LIKE 'fleet/%' OR path IN ('memory/lessons.md', 'memory/judge-feedback.md'))";
