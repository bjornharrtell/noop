//! Which events become a notification, and what it says, honoring the owner's `notify_prefs`. Port of app/notify.ts.
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

const KINDS: [&str; 5] = ["decision", "menu", "proposals", "login", "notice"];

fn title(kind: &str, zh: bool) -> &'static str {
    let (en, cn) = match kind {
        "decision" => ("Needs your call", "需要你拍板"),
        "menu" => ("Pick the next step", "可以选下一步"),
        "proposals" => ("New proposals", "新的提议"),
        "login" => ("Login reminder", "登录提醒"),
        _ => ("Noop", "Noop"),
    };
    if zh { cn } else { en }
}

static RANGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{1,2}):(\d{2})\s*-\s*(\d{1,2}):(\d{2})$").unwrap());
static FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)```.*?```").unwrap());
static MARKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[*_`#>\[\]]").unwrap());
static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(https?:[^)]*\)").unwrap());
static SPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

/// Is `now_ms` inside the quiet hours (`{start, end}` from the app, "23:00-08:00" written by hand) in `tz`?
pub fn in_quiet(range: Option<&Value>, tz: Option<&str>, now_ms: i64) -> bool {
    let text = match range {
        Some(Value::Object(o)) => format!("{}-{}", o.get("start").and_then(Value::as_str).unwrap_or(""), o.get("end").and_then(Value::as_str).unwrap_or("")),
        Some(Value::String(s)) => s.clone(),
        _ => return false,
    };
    let Some(m) = RANGE.captures(&text) else { return false };
    let n = |i: usize| m[i].parse::<i64>().unwrap_or(0);
    let utc = chrono::DateTime::from_timestamp_millis(now_ms).unwrap_or_default();
    let (h, mi) = match tz.filter(|t| !t.is_empty()).and_then(|t| t.parse::<chrono_tz::Tz>().ok()) {
        Some(zone) => {
            let local = utc.with_timezone(&zone);
            (chrono::Timelike::hour(&local) as i64, chrono::Timelike::minute(&local) as i64)
        }
        None => (chrono::Timelike::hour(&utc) as i64, chrono::Timelike::minute(&utc) as i64),
    };
    let t = h * 60 + mi;
    let (a, b) = (n(1) * 60 + n(2), n(3) * 60 + n(4));
    if a <= b { t >= a && t < b } else { t >= a || t < b }
}

fn plain(s: &str) -> String {
    let s = FENCE.replace_all(s, " ");
    let s = MARKS.replace_all(&s, "");
    let s = LINK.replace_all(&s, "");
    SPACE.replace_all(&s, " ").trim().to_string()
}

/// The (title, body) for an event, or None when it should not notify. `prefs` is settings `notify_prefs`.
pub fn notification_for(ev: &Value, prefs: &Value, now_ms: i64) -> Option<(String, String)> {
    let kind = ev.get("kind").and_then(Value::as_str).unwrap_or("");
    if !KINDS.contains(&kind) {
        return None; // never system / owner
    }
    let from = match ev.get("from") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    };
    if kind == "notice" && (from == "hub" || from.starts_with("hub@")) {
        return None; // the hub's own housekeeping
    }
    // settings call notices "results"
    let pref_key = if kind == "notice" { "result" } else { kind };
    if prefs.pointer(&format!("/kinds/{pref_key}")) == Some(&Value::Bool(false)) {
        return None;
    }
    let urgent = kind == "decision" && ev.get("priority").and_then(Value::as_str) == Some("urgent");
    if in_quiet(prefs.get("quiet_hours"), prefs.get("tz").and_then(Value::as_str), now_ms) && kind != "login" && !urgent {
        return None;
    }
    let text = match ev.get("text") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    };
    let body = plain(&text);
    let body = if body.encode_utf16().count() > 140 {
        let mut n = 0;
        let cut: String = body.chars().take_while(|c| { n += c.len_utf16(); n <= 139 }).collect();
        format!("{cut}…")
    } else {
        body
    };
    let zh = prefs.get("lang").and_then(Value::as_str) == Some("zh");
    Some((title(kind, zh).to_string(), body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ms(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis()
    }

    #[test]
    fn rules() {
        let now = ms("2026-11-25T12:00:00Z");
        let none = json!({});
        assert_eq!(notification_for(&json!({ "kind": "decision", "text": "**Ship** [it](https://x.y)?" }), &none, now),
            Some(("Needs your call".into(), "Ship it?".into())));
        assert!(notification_for(&json!({ "kind": "owner", "text": "x" }), &none, now).is_none());
        assert!(notification_for(&json!({ "kind": "notice", "from": "hub", "text": "x" }), &none, now).is_none());
        assert!(notification_for(&json!({ "kind": "notice", "text": "x" }), &json!({ "kinds": { "result": false } }), now).is_none());
        let zh = notification_for(&json!({ "kind": "menu", "text": "x" }), &json!({ "lang": "zh" }), now).unwrap();
        assert_eq!(zh.0, "可以选下一步");
        let long = "a".repeat(200);
        assert_eq!(notification_for(&json!({ "kind": "notice", "text": long }), &none, now).unwrap().1.chars().count(), 140);
    }

    #[test]
    fn quiet_hours() {
        // 23:00 in Copenhagen (UTC+1 in November) is 22:00 UTC
        let night = ms("2026-11-25T22:00:00Z");
        let prefs = json!({ "quiet_hours": { "start": "22:30", "end": "07:00" }, "tz": "Europe/Copenhagen" });
        assert!(in_quiet(prefs.get("quiet_hours"), Some("Europe/Copenhagen"), night));
        assert!(!in_quiet(prefs.get("quiet_hours"), Some("UTC"), night));
        assert!(notification_for(&json!({ "kind": "notice", "text": "x" }), &prefs, night).is_none());
        assert!(notification_for(&json!({ "kind": "login", "text": "x" }), &prefs, night).is_some(), "logins always");
        assert!(notification_for(&json!({ "kind": "decision", "priority": "urgent", "text": "x" }), &prefs, night).is_some());
        assert!(in_quiet(Some(&json!("09:00-17:00")), None, ms("2026-11-25T12:00:00Z")));
    }
}
