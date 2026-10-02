//! Small JSON helpers shared by the Gemini codec modules.
//!
//! Gemini's JSON parser accepts every field in both `camelCase` (the
//! documented REST spelling) and `snake_case` (what the Python SDK and many
//! hand-written clients send), so every lookup goes through [`pick`] with
//! both spellings.

use serde_json::{Map, Value};

/// First non-null value among `names` in the object `v`.
pub(crate) fn pick<'a>(v: &'a Value, names: &[&str]) -> Option<&'a Value> {
    let obj = v.as_object()?;
    pick_in(obj, names)
}

/// Same as [`pick`], on a map.
pub(crate) fn pick_in<'a>(obj: &'a Map<String, Value>, names: &[&str]) -> Option<&'a Value> {
    for name in names {
        if let Some(found) = obj.get(*name)
            && !found.is_null()
        {
            return Some(found);
        }
    }
    None
}

/// First string value among `names`.
pub(crate) fn pick_str<'a>(v: &'a Value, names: &[&str]) -> Option<&'a str> {
    pick(v, names).and_then(Value::as_str)
}

/// Reads a JSON number as `u64`. Integral floats (`12.0`) and decimal strings
/// (proto3 JSON renders 64-bit integers as strings) are accepted.
pub(crate) fn num_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && *f >= 0.0)
                .map(|f| f as u64)
        }),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// Reads a JSON number as `i64`, with the same tolerance as [`num_u64`].
pub(crate) fn num_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Reads a JSON number as `f64`.
pub(crate) fn num_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// `u64` field of an object under any of `names`.
pub(crate) fn pick_u64(v: &Value, names: &[&str]) -> Option<u64> {
    pick(v, names).and_then(num_u64)
}

/// Builds a JSON number from an `f64`, falling back to `null` for NaN/inf.
pub(crate) fn f64_value(f: f64) -> Value {
    serde_json::Number::from_f64(f)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// Removes a `models/` resource prefix from a model name.
pub(crate) fn strip_models_prefix(model: &str) -> &str {
    model.strip_prefix("models/").unwrap_or(model)
}

/// Makes a tool name acceptable to Gemini.
///
/// Gemini function names must start with a letter or an underscore and may
/// contain only `a-z A-Z 0-9 _ . : -`, up to 64 characters. Every other
/// character becomes `_`; a name starting with anything else gets a leading
/// `_`; the result is cut to 64 characters. An empty name stays empty.
///
/// The same function is applied to declarations, to replayed `functionCall`
/// parts, to `functionResponse` names and to `allowedFunctionNames`, so the
/// four always agree.
pub fn sanitize_function_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let starts_ok = out
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !starts_ok {
        // Leave room for the underscore so the name still fits in 64.
        if out.len() >= 64 {
            out.truncate(63);
        }
        out.insert(0, '_');
    }
    out.truncate(64);
    out
}

/// 64-bit FNV-1a with a caller-chosen offset basis.
fn fnv1a64(basis: u64, data: &[u8]) -> u64 {
    let mut hash = basis;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A deterministic tool-call id for a `functionCall` / `functionResponse`
/// part that carries none.
///
/// Gemini clients resend the whole history on every turn. If the ids minted
/// for id-less calls changed between requests, the translated history would
/// differ each time and defeat the upstream's prompt cache, so the id is a
/// pure function of the part's position and content. The result has the same
/// shape as [`switchyard_core::util::new_call_id`].
pub(crate) fn stable_call_id(
    kind: &str,
    content_index: usize,
    part_index: usize,
    name: &str,
    payload: &str,
) -> String {
    let seed = format!("{kind}|{content_index}|{part_index}|{name}|{payload}");
    let a = fnv1a64(0xcbf2_9ce4_8422_2325, seed.as_bytes());
    let b = fnv1a64(0x6c62_272e_07bb_0142, seed.as_bytes());
    format!("call_{a:016x}{:08x}", (b >> 32) as u32)
}

/// Whether `id` has the shape of an id minted by the gateway (`call_` + 24
/// lowercase hex digits) rather than one issued by a provider.
pub(crate) fn is_synthetic_call_id(id: &str) -> bool {
    id.strip_prefix("call_").is_some_and(|rest| {
        rest.len() == 24
            && rest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Parses an RFC 3339 timestamp (`2025-06-01T12:30:00.123Z`) into unix
/// seconds. Returns `None` for anything it does not understand.
pub(crate) fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' {
        return None;
    }
    if !matches!(bytes[10], b'T' | b't' | b' ') || bytes[16] != b':' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| s.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Skip fractional seconds, then read the zone.
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        zone if zone.len() == 6 && (zone.starts_with('+') || zone.starts_with('-')) => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let hh = zone.get(1..3)?.parse::<i64>().ok()?;
            let mm = zone.get(4..6)?.parse::<i64>().ok()?;
            sign * (hh * 3600 + mm * 60)
        }
        _ => return None,
    };
    // Days since the epoch for a proleptic Gregorian date.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// Byte offset → character offset inside `text`, when `byte` falls on a
/// character boundary.
pub(crate) fn byte_to_char_offset(text: &str, byte: usize) -> Option<u64> {
    if byte > text.len() || !text.is_char_boundary(byte) {
        return None;
    }
    Some(text[..byte].chars().count() as u64)
}

/// Character offset → byte offset inside `text` (clamped to the end).
pub(crate) fn char_to_byte_offset(text: &str, chars: u64) -> usize {
    text.char_indices()
        .nth(chars as usize)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pick_prefers_first_non_null_spelling() {
        let v = json!({"inlineData": null, "inline_data": {"a": 1}});
        assert_eq!(
            pick(&v, &["inlineData", "inline_data"]),
            Some(&json!({"a": 1}))
        );
        assert_eq!(pick(&v, &["missing"]), None);
        assert_eq!(pick(&json!("x"), &["a"]), None);
    }

    #[test]
    fn numbers_are_read_liberally() {
        assert_eq!(num_u64(&json!(12)), Some(12));
        assert_eq!(num_u64(&json!(12.0)), Some(12));
        assert_eq!(num_u64(&json!("12")), Some(12));
        assert_eq!(num_u64(&json!(-1)), None);
        assert_eq!(num_i64(&json!(-1)), Some(-1));
        assert_eq!(num_i64(&json!(-1.0)), Some(-1));
        assert_eq!(num_f64(&json!(0.5)), Some(0.5));
        assert_eq!(num_u64(&json!(true)), None);
    }

    #[test]
    fn function_names_follow_gemini_rules() {
        assert_eq!(
            sanitize_function_name("name with spaces"),
            "name_with_spaces"
        );
        assert_eq!(sanitize_function_name("123name"), "_123name");
        assert_eq!(sanitize_function_name("-name"), "_-name");
        assert_eq!(sanitize_function_name("!name"), "_name");
        assert_eq!(sanitize_function_name("@"), "_");
        assert_eq!(
            sanitize_function_name("mcp.server:get-data"),
            "mcp.server:get-data"
        );
        assert_eq!(sanitize_function_name(""), "");
        assert_eq!(sanitize_function_name("名前"), "__");
        let digits = "1".repeat(64);
        let out = sanitize_function_name(&digits);
        assert_eq!(out.len(), 64);
        assert_eq!(out, format!("_{}", "1".repeat(63)));
        let long = "a".repeat(100);
        assert_eq!(sanitize_function_name(&long).len(), 64);
    }

    #[test]
    fn stable_ids_are_deterministic_and_distinct() {
        let a = stable_call_id("call", 1, 0, "f", "{}");
        assert_eq!(a, stable_call_id("call", 1, 0, "f", "{}"));
        assert_ne!(a, stable_call_id("call", 1, 1, "f", "{}"));
        assert_ne!(a, stable_call_id("response", 1, 0, "f", "{}"));
        assert!(is_synthetic_call_id(&a), "{a}");
        assert!(is_synthetic_call_id(&switchyard_core::util::new_call_id()));
        assert!(!is_synthetic_call_id("call_AbCdEfGhIjKlMnOpQrStUvWx"));
        assert!(!is_synthetic_call_id("toolu_01"));
        assert!(!is_synthetic_call_id("call_123"));
    }

    #[test]
    fn rfc3339_parsing() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2024-01-01T00:00:00Z"), Some(1_704_067_200));
        assert_eq!(
            parse_rfc3339("2024-01-01T00:00:00.123456789Z"),
            Some(1_704_067_200)
        );
        assert_eq!(
            parse_rfc3339("2024-01-01T01:00:00+01:00"),
            Some(1_704_067_200)
        );
        assert_eq!(parse_rfc3339("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        assert_eq!(parse_rfc3339("yesterday"), None);
        assert_eq!(parse_rfc3339("2024-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339(""), None);
    }

    #[test]
    fn offsets_convert_between_bytes_and_chars() {
        let text = "héllo wörld";
        assert_eq!(byte_to_char_offset(text, 0), Some(0));
        assert_eq!(byte_to_char_offset(text, 3), Some(2));
        assert_eq!(byte_to_char_offset(text, 2), None);
        assert_eq!(byte_to_char_offset(text, 99), None);
        assert_eq!(char_to_byte_offset(text, 2), 3);
        assert_eq!(char_to_byte_offset(text, 99), text.len());
    }
}
