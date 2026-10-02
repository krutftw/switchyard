//! Session keys for credential affinity.
//!
//! Provider prompt caches are per credential, so the scheduler keeps a
//! conversation on the credential that served it last. What identifies "a
//! conversation" is decided here, from the first of:
//!
//! 1. the key the server passed explicitly (a WebSocket connection id);
//! 2. an `x-session-id` or `session_id` request header;
//! 3. the body's `prompt_cache_key`;
//! 4. the body's `metadata.user_id` (how Anthropic clients name a session);
//! 5. a fingerprint of the conversation's opening — the leading system text
//!    and the first user text — which stays the same on every later turn.
//!
//! The key is always scoped by the client key, so two clients can never
//! share (or steal) a binding.

use http::HeaderMap;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Characters of system and user text that go into the fingerprint.
const FINGERPRINT_PREFIX_CHARS: usize = 512;

/// Longest client-supplied session value used as is; longer ones are hashed
/// so a key never grows with what a client sends.
const MAX_SESSION_CHARS: usize = 128;

/// Request headers that carry a session id.
const SESSION_HEADERS: [&str; 2] = ["x-session-id", "session_id"];

/// The affinity key of a request, or `None` when nothing identifies its
/// conversation.
pub(crate) fn session_key(
    explicit: Option<&str>,
    headers: &HeaderMap,
    body: &Value,
    scope: &str,
) -> Option<String> {
    let from_header = || {
        SESSION_HEADERS.iter().find_map(|name| {
            headers
                .get(*name)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
    };
    let from_body = || {
        non_empty(body.get("prompt_cache_key"))
            .or_else(|| non_empty(body.get("metadata").and_then(|m| m.get("user_id"))))
    };
    let named = explicit
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(from_header)
        .or_else(from_body);
    if let Some(name) = named {
        return Some(if name.chars().count() <= MAX_SESSION_CHARS {
            format!("{scope}:{name}")
        } else {
            format!("{scope}:h-{}", short_hash(&[name]))
        });
    }
    fingerprint(body).map(|print| format!("{scope}:fp-{print}"))
}

fn non_empty(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// First 16 hex digits of the SHA-256 of the parts, each terminated so that
/// `("ab", "c")` and `("a", "bc")` differ.
fn short_hash(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A hash of how the conversation starts, readable from any of the four
/// request shapes without decoding them.
fn fingerprint(body: &Value) -> Option<String> {
    let system = first_system_text(body);
    let user = first_user_text(body);
    if system.is_none() && user.is_none() {
        return None;
    }
    Some(short_hash(&[
        system.as_deref().unwrap_or(""),
        user.as_deref().unwrap_or(""),
    ]))
}

fn prefix(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.chars().take(FINGERPRINT_PREFIX_CHARS).collect())
}

/// The text of a content value: a string, an array of parts with `text`, or
/// an object with `parts` (Gemini) or `text`.
fn text_of(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => prefix(text),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                let piece = match part {
                    Value::String(text) => Some(text.as_str()),
                    other => other.get("text").and_then(Value::as_str),
                };
                if let Some(piece) = piece {
                    out.push_str(piece);
                    if out.chars().count() >= FINGERPRINT_PREFIX_CHARS {
                        break;
                    }
                }
            }
            prefix(&out)
        }
        Value::Object(map) => map
            .get("parts")
            .and_then(text_of)
            .or_else(|| map.get("text").and_then(text_of)),
        _ => None,
    }
}

/// The turns of a request, whatever its protocol calls them.
fn turns(body: &Value) -> impl Iterator<Item = &Value> {
    ["messages", "input", "contents"]
        .into_iter()
        .filter_map(|key| body.get(key).and_then(Value::as_array))
        .flatten()
}

fn role_of(turn: &Value) -> &str {
    turn.get("role").and_then(Value::as_str).unwrap_or("")
}

fn turn_text(turn: &Value) -> Option<String> {
    turn.get("content")
        .and_then(text_of)
        .or_else(|| turn.get("parts").and_then(text_of))
}

fn first_system_text(body: &Value) -> Option<String> {
    [
        "system",
        "instructions",
        "systemInstruction",
        "system_instruction",
    ]
    .into_iter()
    .find_map(|key| body.get(key).and_then(text_of))
    .or_else(|| {
        turns(body)
            .find(|turn| matches!(role_of(turn), "system" | "developer"))
            .and_then(turn_text)
    })
}

fn first_user_text(body: &Value) -> Option<String> {
    // Responses accepts a bare string as the whole input.
    if let Some(Value::String(input)) = body.get("input") {
        return prefix(input);
    }
    turns(body)
        .find(|turn| role_of(turn) == "user")
        .and_then(turn_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use serde_json::json;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_static(value));
        }
        map
    }

    #[test]
    fn precedence_explicit_header_cache_key_user_id_fingerprint() {
        let body = json!({
            "prompt_cache_key": "cache-1",
            "metadata": {"user_id": "user-1"},
            "messages": [{"role": "user", "content": "hello"}]
        });
        let h = headers(&[("x-session-id", "sess-h")]);
        assert_eq!(
            session_key(Some("ws-7"), &h, &body, "k1").as_deref(),
            Some("k1:ws-7")
        );
        assert_eq!(
            session_key(None, &h, &body, "k1").as_deref(),
            Some("k1:sess-h")
        );
        assert_eq!(
            session_key(None, &headers(&[("session_id", "sess-u")]), &body, "k1").as_deref(),
            Some("k1:sess-u")
        );
        assert_eq!(
            session_key(None, &HeaderMap::new(), &body, "k1").as_deref(),
            Some("k1:cache-1")
        );
        let body = json!({"metadata": {"user_id": "user-1"}, "messages": []});
        assert_eq!(
            session_key(None, &HeaderMap::new(), &body, "k1").as_deref(),
            Some("k1:user-1")
        );
        // Blank values do not count.
        let body = json!({"prompt_cache_key": "  ", "metadata": {"user_id": "user-1"}});
        assert_eq!(
            session_key(Some(" "), &headers(&[("x-session-id", "")]), &body, "k1").as_deref(),
            Some("k1:user-1")
        );
    }

    #[test]
    fn keys_are_scoped_by_client() {
        let body = json!({"prompt_cache_key": "same"});
        let a = session_key(None, &HeaderMap::new(), &body, "key-a").unwrap();
        let b = session_key(None, &HeaderMap::new(), &body, "key-b").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn overlong_client_values_are_hashed() {
        let long = "s".repeat(5000);
        let key = session_key(Some(&long), &HeaderMap::new(), &json!({}), "k").unwrap();
        assert!(key.len() < 40, "{key}");
        assert!(key.starts_with("k:h-"));
        assert_eq!(
            session_key(Some(&long), &HeaderMap::new(), &json!({}), "k").unwrap(),
            key
        );
    }

    #[test]
    fn nothing_to_go_on_means_no_session() {
        for body in [json!({}), json!({"messages": []}), json!(null), json!("x")] {
            assert_eq!(session_key(None, &HeaderMap::new(), &body, "k"), None);
        }
    }

    #[test]
    fn the_fingerprint_is_stable_across_turns_and_differs_between_conversations() {
        let turn1 = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "You are terse."},
                {"role": "user", "content": "What is Rust?"}
            ]
        });
        let turn2 = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "You are terse."},
                {"role": "user", "content": "What is Rust?"},
                {"role": "assistant", "content": "A language."},
                {"role": "user", "content": "And Go?"}
            ]
        });
        let other = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "You are terse."},
                {"role": "user", "content": "What is Zig?"}
            ]
        });
        let k1 = session_key(None, &HeaderMap::new(), &turn1, "k").unwrap();
        assert!(k1.starts_with("k:fp-"));
        assert_eq!(
            session_key(None, &HeaderMap::new(), &turn2, "k").unwrap(),
            k1
        );
        assert_ne!(
            session_key(None, &HeaderMap::new(), &other, "k").unwrap(),
            k1
        );
    }

    #[test]
    fn every_protocols_opening_is_found() {
        // Chat: content parts.
        let chat = json!({"messages": [
            {"role": "developer", "content": [{"type": "text", "text": "sys"}]},
            {"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "image_url"}]}
        ]});
        assert_eq!(first_system_text(&chat).as_deref(), Some("sys"));
        assert_eq!(first_user_text(&chat).as_deref(), Some("hi"));

        // Responses: instructions + input items, or a bare string.
        let responses = json!({"instructions": "sys", "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
        ]});
        assert_eq!(first_system_text(&responses).as_deref(), Some("sys"));
        assert_eq!(first_user_text(&responses).as_deref(), Some("hi"));
        assert_eq!(
            first_user_text(&json!({"input": "just text"})).as_deref(),
            Some("just text")
        );

        // Anthropic: system as a string or as blocks.
        let anthropic = json!({
            "system": [{"type": "text", "text": "sys"}],
            "messages": [{"role": "user", "content": "hi"}]
        });
        assert_eq!(first_system_text(&anthropic).as_deref(), Some("sys"));
        assert_eq!(first_user_text(&anthropic).as_deref(), Some("hi"));
        assert_eq!(
            first_system_text(&json!({"system": "plain"})).as_deref(),
            Some("plain")
        );

        // Gemini: systemInstruction and contents with parts.
        let gemini = json!({
            "systemInstruction": {"parts": [{"text": "sys"}]},
            "contents": [
                {"role": "model", "parts": [{"text": "earlier"}]},
                {"role": "user", "parts": [{"text": "hi"}, {"inlineData": {}}]}
            ]
        });
        assert_eq!(first_system_text(&gemini).as_deref(), Some("sys"));
        assert_eq!(first_user_text(&gemini).as_deref(), Some("hi"));
    }

    #[test]
    fn long_openings_are_cut_to_a_prefix() {
        let long = "x".repeat(10_000);
        let a = json!({"messages": [{"role": "user", "content": long}]});
        let mut longer = "x".repeat(10_000);
        longer.push_str("tail that is beyond the prefix");
        let b = json!({"messages": [{"role": "user", "content": longer}]});
        assert_eq!(
            session_key(None, &HeaderMap::new(), &a, "k"),
            session_key(None, &HeaderMap::new(), &b, "k")
        );
    }
}
