//! Reading and normalising single values of the source document.

use serde_json::{Map, Value};
use std::collections::HashSet;
use switchyard_core::config::{ModelConfig, Strategy};
use switchyard_core::{Effort, ThinkingSupport};

pub(super) fn lookup<'v>(root: &'v Map<String, Value>, path: &str) -> Option<&'v Value> {
    let mut segments = path.split('.');
    let mut current = root.get(segments.next()?)?;
    for segment in segments {
        current = current.as_object()?.get(segment)?;
    }
    Some(current)
}

/// The value of a setting with a nested and a flat spelling. Presence of the
/// nested one decides, whatever its value; null counts as not set.
pub(super) fn pick_in<'v>(
    root: &'v Map<String, Value>,
    nested: &str,
    flat: &str,
) -> Option<&'v Value> {
    match lookup(root, nested) {
        Some(value) => Some(value),
        None => lookup(root, flat),
    }
    .filter(|value| !value.is_null())
}

/// A scalar as text, trimmed.
///
/// The source program reads these settings as text and gets what the file
/// says, quoted or not. The document is read verbatim (see
/// [`crate::yaml::parse_verbatim`]), so an unquoted number or boolean that
/// arrives here typed is spelt exactly as its text: `8317` is `"8317"`,
/// and `012345`, which is not canonical, was never typed in the first place.
pub(super) fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// A boolean: `true` and `false`, and the YAML 1.1 words the source program
/// still accepts where it expects one.
pub(super) fn boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::String(text) => match text.trim() {
            "y" | "Y" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => Some(true),
            "n" | "N" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => Some(false),
            other => match other.to_ascii_lowercase().as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
        },
        _ => None,
    }
}

pub(super) fn integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64().or_else(|| whole(number.as_f64()?)),
        Value::String(text) => integer_text(text.trim()),
        _ => None,
    }
}

/// A float that is a whole number an `i64` holds exactly.
fn whole(number: f64) -> Option<i64> {
    (number.is_finite() && number.fract() == 0.0 && number.abs() < 9.0e15).then_some(number as i64)
}

/// An integer written as text, the spellings the source program's YAML
/// reader takes for one: decimal, `0x` hexadecimal, `0o` (or a leading
/// zero) octal, `0b` binary, `_` between digits, a sign, and a float that
/// is a whole number (`1e3`).
fn integer_text(text: &str) -> Option<i64> {
    let plain: String = text.chars().filter(|c| *c != '_').collect();
    let (negative, digits) = match plain.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, plain.strip_prefix('+').unwrap_or(&plain)),
    };
    let radix = |digits: &str, radix: u32| {
        (!digits.is_empty() && digits.chars().all(|c| c.is_digit(radix)))
            .then(|| i64::from_str_radix(digits, radix).ok())
            .flatten()
    };
    let prefixed = |lower: &str, upper: &str| {
        digits
            .strip_prefix(lower)
            .or_else(|| digits.strip_prefix(upper))
    };
    let magnitude = if let Some(hex) = prefixed("0x", "0X") {
        radix(hex, 16)
    } else if let Some(octal) = prefixed("0o", "0O") {
        radix(octal, 8)
    } else if let Some(binary) = prefixed("0b", "0B") {
        radix(binary, 2)
    } else if digits.len() > 1 && digits.starts_with('0') && radix(digits, 8).is_some() {
        radix(digits, 8)
    } else {
        radix(digits, 10)
    };
    if let Some(magnitude) = magnitude {
        return if negative {
            magnitude.checked_neg()
        } else {
            Some(magnitude)
        };
    }
    // Not `inf`, `nan` and the like, which Rust would parse as floats.
    let float_like = plain.chars().any(|c| c.is_ascii_digit())
        && plain
            .chars()
            .all(|c| c.is_ascii_digit() || "+-.eE".contains(c));
    if float_like {
        whole(plain.parse::<f64>().ok()?)
    } else {
        None
    }
}

pub(super) fn clamp_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

pub(super) fn clamp_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

pub(super) fn clamp_i32(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX })
}

/// A list of strings: trimmed, empty ones dropped, first occurrence kept.
pub(super) fn string_list(value: &Value) -> Vec<String> {
    let mut seen = HashSet::new();
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(text)
                .filter(|item| !item.is_empty() && seen.insert(item.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether the source program would treat the secret as already hashed.
pub(super) fn is_bcrypt_hash(secret: &str) -> bool {
    secret.len() > 4
        && ["$2a$", "$2b$", "$2y$"]
            .iter()
            .any(|p| secret.starts_with(p))
}

pub(super) fn strategy(name: &str) -> Option<Strategy> {
    match name.trim().to_ascii_lowercase().as_str() {
        "" | "round-robin" | "roundrobin" | "rr" => Some(Strategy::RoundRobin),
        "weighted-round-robin" | "weightedroundrobin" | "wrr" | "weighted" => {
            Some(Strategy::Weighted)
        }
        "fill-first" | "fillfirst" | "ff" => Some(Strategy::FillFirst),
        _ => None,
    }
}

/// Seconds of a Go duration string (`1h`, `30m`, `1h30m`, `90s`, `500ms`).
pub(super) fn go_duration_secs(text: &str) -> Option<f64> {
    let mut rest = text.trim();
    if rest.is_empty() {
        return None;
    }
    let mut total = 0.0;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let number: f64 = rest.get(..digits)?.parse().ok()?;
        rest = rest.get(digits..)?;
        let unit = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let factor = match rest.get(..unit)? {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 1e-3,
            "us" | "µs" => 1e-6,
            "ns" => 1e-9,
            _ => return None,
        };
        total += number * factor;
        rest = rest.get(unit..)?;
    }
    Some(total)
}

/// A prefix as the source program normalises it: trimmed, without leading
/// or trailing slashes, and dropped entirely when a slash remains inside.
pub(super) fn normalize_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.contains('/') {
        String::new()
    } else {
        trimmed.to_string()
    }
}

/// A provider name from free text: lowercase letters, digits, `-` and `_`.
pub(super) fn sanitize_name(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed: String = out.trim_matches('-').chars().take(48).collect();
    trimmed.trim_matches('-').to_string()
}

pub(super) fn unique_name(base: &str, used: &mut HashSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    let mut n = 2usize;
    loop {
        let candidate = format!("{base}-{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

/// Values that configure nothing: null, `false`, `0`, empty text, empty
/// collections. Text counts when it is another spelling of `false` or of
/// zero (`False`, `00`, `0x0`), which the verbatim reading leaves as text.
pub(super) fn is_zero(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !flag,
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::String(text) => {
            let text = text.trim();
            text.is_empty() || matches!(text, "False" | "FALSE") || integer_text(text) == Some(0)
        }
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}

pub(super) fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Whether a gjson path is also a path of Switchyard's payload rules: keys
/// and list positions separated by dots, `\.` for a literal dot, and none of
/// gjson's queries, wildcards or modifiers.
pub(super) fn is_simple_path(path: &str) -> bool {
    let path = path.trim();
    if path.is_empty() || path.starts_with('.') || path.ends_with('.') || path.contains("..") {
        return false;
    }
    // Only a dot may be escaped: gjson escapes its other special characters
    // the same way, and those have no meaning here.
    let mut escaped = false;
    for c in path.chars() {
        if escaped {
            if c != '.' {
                return false;
            }
            escaped = false;
            continue;
        }
        match c {
            '#' | '*' | '?' | '|' | '@' | '!' => return false,
            '\\' => escaped = true,
            _ => {}
        }
    }
    !escaped
}

pub(super) fn thinking(value: &Value) -> Option<ThinkingSupport> {
    let map = value.as_object()?;
    let mut support = ThinkingSupport::default();
    if let Some(levels) = map.get("levels").and_then(Value::as_array) {
        for level in levels.iter().filter_map(text) {
            let level = level.to_ascii_lowercase();
            match level.as_str() {
                "none" => support.zero_allowed = true,
                "auto" => support.dynamic_allowed = true,
                _ => {
                    if let Ok(effort) = serde_json::from_value::<Effort>(Value::String(level))
                        && !support.levels.contains(&effort)
                    {
                        support.levels.push(effort);
                    }
                }
            }
        }
    }
    if let Some(min) = map.get("min").and_then(integer) {
        support.min = clamp_u32(min);
    }
    if let Some(max) = map.get("max").and_then(integer) {
        support.max = clamp_u32(max);
    }
    if map.get("zero-allowed").and_then(boolean) == Some(true) {
        support.zero_allowed = true;
    }
    if map.get("dynamic-allowed").and_then(boolean) == Some(true) {
        support.dynamic_allowed = true;
    }
    (support != ThinkingSupport::default()).then_some(support)
}

/// Keeps the first model of each client-facing name (case-insensitive), as
/// the source program does for these provider families.
pub(super) fn dedupe_models(models: Vec<ModelConfig>) -> Vec<ModelConfig> {
    let mut seen = HashSet::new();
    models
        .into_iter()
        .filter(|model| seen.insert(model.client_name().to_ascii_lowercase()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_what_the_file_says() {
        use serde_json::json;
        assert_eq!(text(&json!("  sk-key  ")).as_deref(), Some("sk-key"));
        assert_eq!(text(&json!("012345")).as_deref(), Some("012345"));
        assert_eq!(text(&json!(8317)).as_deref(), Some("8317"));
        assert_eq!(text(&json!(-2.5)).as_deref(), Some("-2.5"));
        assert_eq!(text(&json!(true)).as_deref(), Some("true"));
        assert_eq!(text(&json!(null)), None);
        assert_eq!(text(&json!(["a"])), None);
        assert_eq!(text(&json!({"a": 1})), None);
        assert_eq!(
            string_list(&json!(["a", " a ", 7, "007", true, null, "", ["x"]])),
            ["a", "7", "007", "true"]
        );
    }

    #[test]
    fn integers_in_every_spelling() {
        use serde_json::json;
        for (written, expected) in [
            (json!(8317), Some(8317)),
            (json!(-3), Some(-3)),
            (json!(1000.0), Some(1000)),
            (json!(2.5), None),
            (json!("8317"), Some(8317)),
            (json!(" 42 "), Some(42)),
            (json!("+5"), Some(5)),
            (json!("-5"), Some(-5)),
            (json!("0x1F"), Some(31)),
            (json!("-0x10"), Some(-16)),
            (json!("0o17"), Some(15)),
            (json!("0b101"), Some(5)),
            // A leading zero is octal there, unless the digits are not.
            (json!("0777"), Some(511)),
            (json!("08"), Some(8)),
            (json!("00"), Some(0)),
            (json!("1_000"), Some(1000)),
            (json!("1e3"), Some(1000)),
            (json!("2.0"), Some(2)),
            (json!("2.5"), None),
            (json!("1e400"), None),
            (json!("99999999999999999999"), None),
            (json!("0x"), None),
            (json!("0x-5"), None),
            (json!("inf"), None),
            (json!("nan"), None),
            (json!("-"), None),
            (json!(""), None),
            (json!("ten"), None),
            (json!(true), None),
            (json!(null), None),
        ] {
            assert_eq!(integer(&written), expected, "{written}");
        }
    }

    #[test]
    fn booleans_and_zero_values() {
        use serde_json::json;
        for yes in ["true", "True", "TRUE", "yes", "Yes", "on", "ON", "y"] {
            assert_eq!(boolean(&json!(yes)), Some(true), "{yes}");
        }
        for no in ["false", "False", "no", "NO", "off", "Off", "n"] {
            assert_eq!(boolean(&json!(no)), Some(false), "{no}");
        }
        assert_eq!(boolean(&json!(true)), Some(true));
        for neither in [json!("yEs"), json!("1"), json!(1), json!(null), json!("")] {
            assert_eq!(boolean(&neither), None, "{neither}");
        }
        for zero in [
            json!(null),
            json!(false),
            json!(0),
            json!(0.0),
            json!(""),
            json!("  "),
            json!("False"),
            json!("00"),
            json!("0x0"),
            json!([]),
            json!({}),
        ] {
            assert!(is_zero(&zero), "{zero}");
        }
        for set in [json!(true), json!(1), json!("x"), json!("01"), json!([0])] {
            assert!(!is_zero(&set), "{set}");
        }
    }

    #[test]
    fn go_durations() {
        assert_eq!(go_duration_secs("1h"), Some(3600.0));
        assert_eq!(go_duration_secs("1h30m"), Some(5400.0));
        assert_eq!(go_duration_secs("90s"), Some(90.0));
        assert_eq!(go_duration_secs("1.5m"), Some(90.0));
        assert_eq!(go_duration_secs("500ms"), Some(0.5));
        assert_eq!(go_duration_secs(""), None);
        assert_eq!(go_duration_secs("soon"), None);
        assert_eq!(go_duration_secs("10"), None);
        assert_eq!(go_duration_secs("10 parsecs"), None);
    }

    #[test]
    fn names_are_sanitised_and_unique() {
        assert_eq!(sanitize_name("OpenRouter"), "openrouter");
        assert_eq!(sanitize_name("  My Local LLM!  "), "my-local-llm");
        assert_eq!(sanitize_name("a__b--c"), "a__b-c");
        assert_eq!(sanitize_name("✓✓✓"), "");
        assert_eq!(sanitize_name(&"x".repeat(100)).len(), 48);
        let mut used = HashSet::new();
        assert_eq!(unique_name("gemini", &mut used), "gemini");
        assert_eq!(unique_name("gemini", &mut used), "gemini-2");
        assert_eq!(unique_name("gemini", &mut used), "gemini-3");
        assert_eq!(unique_name("gemini-2", &mut used), "gemini-2-2");
    }

    #[test]
    fn strategies_and_hashes() {
        assert_eq!(strategy("WRR"), Some(Strategy::Weighted));
        assert_eq!(strategy("fillfirst"), Some(Strategy::FillFirst));
        assert_eq!(strategy(" round-robin "), Some(Strategy::RoundRobin));
        assert_eq!(strategy("random"), None);
        assert!(is_bcrypt_hash("$2a$10$abcdefghijklmnopqrstuv"));
        assert!(!is_bcrypt_hash("$2a$"));
        assert!(!is_bcrypt_hash("plain-secret"));
    }

    #[test]
    fn simple_paths() {
        for path in ["a", "a.b.0.c", "generationConfig.thinkingConfig", "a\\.b.c"] {
            assert!(is_simple_path(path), "{path}");
        }
        for path in [
            "",
            "tools.#(name==\"x\")",
            "a.*.b",
            "a.#",
            "a|@reverse",
            "a..b",
            ".a",
            "a.",
            "a\\*b",
            "a?",
        ] {
            assert!(!is_simple_path(path), "{path}");
        }
    }
}
