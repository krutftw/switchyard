//! Small shared helpers: identifiers, time, wildcard matching, secret masking.

use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// Current time as unix seconds.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current time as unix milliseconds.
pub fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 24 random lowercase hex characters.
pub fn random_hex24() -> String {
    let mut s = uuid::Uuid::new_v4().simple().to_string();
    s.truncate(24);
    s
}

/// A fresh identifier with the given prefix, e.g. `new_id("chatcmpl-")`.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", random_hex24())
}

/// A fresh tool-call id (`call_…`) for protocols that do not supply one.
pub fn new_call_id() -> String {
    new_id("call_")
}

/// Case-insensitive wildcard match where `*` matches any run of characters
/// (including none). There is no escape syntax and no other metacharacter.
///
/// `gpt-*` matches `gpt-5`, `*-preview` matches `gemini-3-pro-preview`,
/// `*flash*` matches `gemini-2.5-flash-lite`, `*` matches everything.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
    let t: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Masks a secret for display: keeps a short prefix and suffix, hides the
/// rest. The shorter the secret the less is shown — never more than about a
/// quarter of it — and secrets of up to 11 characters are hidden entirely.
pub fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    let n = chars.len();
    if n == 0 {
        return String::new();
    }
    if n <= 11 {
        return "•".repeat(n.min(8));
    }
    let (keep_start, keep_end) = match n {
        12..=15 => (2, 1),
        16..=23 => (3, 2),
        24..=31 => (4, 3),
        _ => (6, 4),
    };
    let head: String = chars[..keep_start].iter().collect();
    let tail: String = chars[n - keep_end..].iter().collect();
    format!("{head}…{tail}")
}

/// Truncates text to at most `max_chars` characters, appending `…` when cut.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Reads a JSON value as a string, accepting strings only.
pub fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Reads a JSON number as `u64`, accepting integers and integral floats
/// (some providers serialise token counts as `12.0`).
pub fn u64_field(value: &Value, key: &str) -> Option<u64> {
    let v = value.get(key)?;
    v.as_u64().or_else(|| {
        v.as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|f| f as u64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_prefixed() {
        let a = new_id("msg_");
        let b = new_id("msg_");
        assert!(a.starts_with("msg_") && a.len() == 28);
        assert_ne!(a, b);
        assert!(new_call_id().starts_with("call_"));
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("gpt-*", "gpt-5"));
        assert!(wildcard_match("gpt-*", "GPT-5-mini"));
        assert!(!wildcard_match("gpt-*", "o3-gpt"));
        assert!(wildcard_match("*-preview", "gemini-3-pro-preview"));
        assert!(!wildcard_match("*-preview", "gemini-3-pro"));
        assert!(wildcard_match("*flash*", "gemini-2.5-flash-lite"));
        assert!(wildcard_match("a*b*c", "a123b456c"));
        assert!(!wildcard_match("a*b*c", "a123b456"));
        assert!(wildcard_match("exact", "exact"));
        assert!(!wildcard_match("exact", "exact-not"));
        assert!(wildcard_match("**", "x"));
        assert!(!wildcard_match("", "x"));
        assert!(wildcard_match("", ""));
    }

    #[test]
    fn masking() {
        assert_eq!(mask_secret(""), "");
        assert_eq!(mask_secret("short"), "•••••");
        assert_eq!(mask_secret("elevenchars"), "••••••••");
        assert_eq!(mask_secret("sk-abcdefghijk"), "sk…k");
        assert_eq!(mask_secret("sk-abcdefghijklmnop"), "sk-…op");
        assert_eq!(mask_secret("sk-abcdefghijklmnopqrstuv"), "sk-a…tuv");
        assert_eq!(
            mask_secret("sk-proj-abcdefghijklmnopqrstuvwxyz"),
            "sk-pro…wxyz"
        );
        // Never reveal more than about a quarter of the secret.
        for n in 1..80usize {
            let secret: String = (0..n).map(|i| (b'a' + (i % 26) as u8) as char).collect();
            let shown = mask_secret(&secret)
                .chars()
                .filter(|c| c.is_ascii_alphabetic())
                .count();
            assert!(shown * 3 <= n, "{n} chars: {shown} shown");
        }
    }

    #[test]
    fn numeric_fields() {
        let v = serde_json::json!({"a": 3, "b": 4.0, "c": -1, "d": "5"});
        assert_eq!(u64_field(&v, "a"), Some(3));
        assert_eq!(u64_field(&v, "b"), Some(4));
        assert_eq!(u64_field(&v, "c"), None);
        assert_eq!(u64_field(&v, "d"), None);
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello", 3), "hel…");
    }
}
