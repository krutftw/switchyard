//! Error envelopes: rendering [`ApiError`] for clients, parsing upstream
//! error bodies, and turning in-stream failure payloads into [`ApiError`]s.
//!
//! Error text that comes from an upstream is never trusted to be free of
//! credentials: a proxy in front of the vendor may echo the request's
//! `Authorization` header or key in its complaint. Everything that enters
//! through [`decode_error`] / [`api_error_from_stream`] and every message
//! that leaves through [`error_detail`] goes through [`redact_secrets`].

use crate::common::non_empty;
use serde_json::{Map, Value, json};
use switchyard_core::util::{str_field, truncate_chars, u64_field};
use switchyard_core::{ApiError, ErrorKind, UpstreamErrorInfo};

/// Longest message kept from an upstream error body.
const MAX_MESSAGE_CHARS: usize = 2000;

/// Longest error type / code / param kept from an upstream error body.
const MAX_FIELD_CHARS: usize = 256;

/// What a credential is replaced with.
const REDACTED: &str = "[REDACTED]";

// ---------------------------------------------------------------------------
// Credential redaction
// ---------------------------------------------------------------------------

/// Names whose assigned value (`name=value`, `name: value`, `"name":"value"`)
/// is a credential. Matching is by substring, so `token` also covers
/// `access_token`, `refresh_token`, `id_token` and the like; plurals and
/// counters (`max_tokens: 5`, `token_count: 5`) do not match because the
/// separator must follow the name directly.
const SECRET_NAMES: &[&str] = &[
    "authorization",
    "api_key",
    "api-key",
    "apikey",
    "token",
    "secret",
    "password",
];

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_quote(byte: Option<&u8>) -> bool {
    matches!(byte, Some(b'"' | b'\''))
}

fn skip_whitespace(bytes: &[u8], mut at: usize) -> usize {
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    at
}

/// Length in bytes of the value that starts at `text`: everything up to
/// whitespace or a character that ends a value in a query string, a header
/// dump or a JSON document.
fn value_len(text: &str) -> usize {
    text.char_indices()
        .find(|(_, c)| c.is_whitespace() || matches!(c, '"' | '\'' | '&' | ',' | ';' | '}'))
        .map_or(text.len(), |(offset, _)| offset)
}

/// `Bearer <token>` becomes `Bearer [REDACTED]`. A purely alphabetic word
/// after `Bearer` ("Bearer auth", "bearer token") is prose, not a credential,
/// and is left alone so the vendor's own guidance stays readable.
fn redact_bearer(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(found) = lower[from..].find("bearer") {
        let start = from + found;
        let after = start + "bearer".len();
        from = after;
        if start > 0 && is_word_byte(bytes[start - 1]) {
            continue;
        }
        let token_start = skip_whitespace(bytes, after);
        if token_start == after {
            continue;
        }
        // The token68 alphabet plus `_` (base64url, `sk_live_…` style keys).
        let run = bytes[token_start..]
            .iter()
            .take_while(|b| is_word_byte(**b) || b".~+/=-".contains(b))
            .count();
        // A full stop that ends the sentence is not part of the token.
        let token = text[token_start..token_start + run].trim_end_matches('.');
        let looks_secret = token.len() >= 24 || token.bytes().any(|b| !b.is_ascii_alphabetic());
        if token.is_empty() || !looks_secret {
            continue;
        }
        out.push_str(&text[copied..token_start]);
        out.push_str(REDACTED);
        copied = token_start + token.len();
        from = copied;
    }
    out.push_str(&text[copied..]);
    out
}

/// `<secret name> [=:] <value>`: the value becomes `[REDACTED]`. Optional
/// quotes around the name and the value are tolerated, which also covers
/// secret-named keys of a JSON document rendered as text. For
/// `authorization: <scheme> <credential>` the credential is what is hidden.
fn redact_assignments(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut at = 0;
    while at < bytes.len() {
        let name = SECRET_NAMES
            .iter()
            .find(|name| bytes[at..].starts_with(name.as_bytes()));
        let name_len = match name {
            Some(name) => name.len(),
            // `?key=...` / `&key=...`: the query-string spelling of an API key.
            None if at > 0
                && matches!(bytes[at - 1], b'?' | b'&')
                && bytes[at..].starts_with(b"key=") =>
            {
                "key".len()
            }
            None => {
                at += 1;
                continue;
            }
        };
        let mut cursor = at + name_len;
        at += 1;
        if is_quote(bytes.get(cursor)) {
            cursor += 1;
        }
        cursor = skip_whitespace(bytes, cursor);
        if !matches!(bytes.get(cursor), Some(b'=' | b':')) {
            continue;
        }
        cursor = skip_whitespace(bytes, cursor + 1);
        let quote = bytes.get(cursor).copied().filter(|b| is_quote(Some(b)));
        if quote.is_some() {
            cursor += 1;
        }
        // Only ASCII has been consumed since the name started, so `cursor`
        // is a character boundary of `text`. A quoted value runs to its
        // closing quote, a bare one to the next separator.
        let quoted_len = quote.and_then(|quote| {
            (cursor..bytes.len()).find(|&i| bytes[i] == quote && bytes[i - 1] != b'\\')
        });
        let (mut start, mut end) = match quoted_len {
            Some(close) => (cursor, close),
            None => (cursor, cursor + value_len(&text[cursor..])),
        };
        if start == end {
            continue;
        }
        let scheme = &lower[start..end];
        if scheme == "bearer" || scheme == "basic" {
            let credential = skip_whitespace(bytes, end);
            let len = value_len(&text[credential..]);
            if credential == end || len == 0 {
                at = end;
                continue;
            }
            (start, end) = (credential, credential + len);
        }
        if start >= copied && !text[start..end].starts_with(REDACTED) {
            out.push_str(&text[copied..start]);
            out.push_str(REDACTED);
            copied = end;
        }
        at = end.max(at);
    }
    out.push_str(&text[copied..]);
    out
}

/// Bare keys recognisable by their shape: prefix, characters the key
/// continues with besides ASCII letters and digits, and the shortest run
/// after the prefix that is taken for a key (keys the vendor already masked,
/// `sk-proj-****abcd`, stay below it).
///
/// * `sk-…`: OpenAI and most compatible servers;
/// * `AIza…`: Google API keys (a relay speaking this protocol may front any
///   vendor);
/// * `ya29.…`: Google OAuth access tokens.
const KEY_SHAPES: &[(&str, &[u8], usize)] = &[
    ("sk-", b"_-", 20),
    ("AIza", b"_-", 30),
    ("ya29.", b"_-.", 20),
];

/// Bare keys of one of the [`KEY_SHAPES`], wherever they appear.
fn redact_key_shape(text: &str, prefix: &str, extra: &[u8], min_run: usize) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(found) = text[from..].find(prefix) {
        let start = from + found;
        let body = start + prefix.len();
        from = body;
        if start > 0 && is_word_byte(bytes[start - 1]) {
            continue;
        }
        let run = bytes[body..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || extra.contains(b))
            .count();
        if run < min_run {
            continue;
        }
        // A full stop that ends the sentence is not part of the key.
        let end = body + text[body..body + run].trim_end_matches('.').len();
        out.push_str(&text[copied..start]);
        out.push_str(REDACTED);
        copied = end;
        from = copied;
    }
    out.push_str(&text[copied..]);
    out
}

/// Removes credentials from error text: `Bearer` tokens, values assigned to
/// secret-looking names, and bare vendor keys. Idempotent.
pub(crate) fn redact_secrets(text: &str) -> String {
    let mut out = redact_assignments(&redact_bearer(text));
    for (prefix, extra, min_run) in KEY_SHAPES {
        if out.contains(prefix) {
            out = redact_key_shape(&out, prefix, extra, *min_run);
        }
    }
    out
}

/// Redacts and truncates a piece of upstream error text.
fn sanitize(text: &str, max_chars: usize) -> String {
    truncate_chars(&redact_secrets(text), max_chars)
}

/// [`sanitize`] with the limit for error messages.
pub(crate) fn sanitize_message(text: &str) -> String {
    sanitize(text, MAX_MESSAGE_CHARS)
}

// ---------------------------------------------------------------------------
// Client-facing envelopes
// ---------------------------------------------------------------------------

/// OpenAI `error.type` and default `error.code` for an error kind.
pub(crate) fn type_and_code(kind: ErrorKind) -> (&'static str, Option<&'static str>) {
    match kind {
        ErrorKind::InvalidRequest => ("invalid_request_error", None),
        ErrorKind::Authentication => ("authentication_error", Some("invalid_api_key")),
        ErrorKind::Permission => ("permission_error", None),
        ErrorKind::NotFound => ("invalid_request_error", None),
        ErrorKind::TooLarge => ("invalid_request_error", Some("request_too_large")),
        ErrorKind::RateLimit => ("rate_limit_error", Some("rate_limit_exceeded")),
        ErrorKind::Upstream => ("server_error", Some("upstream_error")),
        ErrorKind::Unavailable => ("service_unavailable_error", Some("service_unavailable")),
        ErrorKind::Timeout => ("server_error", Some("request_timeout")),
        ErrorKind::Internal => ("server_error", Some("internal_server_error")),
    }
}

/// The `{message, type, param, code}` object shared by the HTTP error body,
/// the SSE `error` event and failed response objects.
///
/// The message is redacted once more on the way out. Errors decoded by this
/// crate are already clean, but one that another protocol's decoder took
/// from an upstream stream need not be, and this is the last place to catch
/// it before it reaches a client.
pub(crate) fn error_detail(error: &ApiError) -> Value {
    let (kind, default_code) = type_and_code(error.kind);
    json!({
        "message": redact_secrets(&error.message),
        "type": kind,
        "param": error.param,
        "code": error.code.as_deref().or(default_code),
    })
}

/// Renders an error as the OpenAI HTTP error body.
pub(crate) fn encode_error(error: &ApiError) -> Value {
    json!({"error": error_detail(error)})
}

// ---------------------------------------------------------------------------
// Upstream error bodies
// ---------------------------------------------------------------------------

/// Parses an upstream error body. Never fails: bodies that are not JSON are
/// summarised as text.
pub(crate) fn decode_error(status: u16, body: &[u8]) -> UpstreamErrorInfo {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return UpstreamErrorInfo {
            message: format!("upstream returned HTTP {status} with an empty body"),
            ..UpstreamErrorInfo::default()
        };
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed)
        && let Some(info) = info_from_json(&value)
    {
        return info;
    }
    let message = if looks_like_html(trimmed) {
        match html_title(trimmed) {
            Some(title) => sanitize_message(&format!("upstream returned HTTP {status}: {title}")),
            None => format!("upstream returned HTTP {status} with an HTML error page"),
        }
    } else {
        sanitize_message(trimmed)
    };
    let retry_after_ms = retry_hint_ms(trimmed);
    UpstreamErrorInfo {
        message,
        error_type: None,
        code: None,
        retry_after_ms,
    }
}

/// Locates the error object inside the many shapes "OpenAI-compatible"
/// servers use.
fn find_detail(value: &Value) -> Option<&Value> {
    match value {
        // Google-style wrappers put the envelope in a one-element array.
        Value::Array(items) => items.first().and_then(find_detail),
        Value::Object(map) => {
            if let Some(error) = map.get("error").filter(|v| !v.is_null()) {
                return Some(error);
            }
            if let Some(error) = map
                .get("response")
                .and_then(|r| r.get("error"))
                .filter(|v| !v.is_null())
            {
                return Some(error);
            }
            if map.contains_key("message") || map.contains_key("detail") {
                return Some(value);
            }
            None
        }
        _ => None,
    }
}

/// A short scalar field of an upstream error (type, code), cleaned like the
/// message.
fn scalar_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) if !text.trim().is_empty() => {
            Some(sanitize(text.trim(), MAX_FIELD_CHARS))
        }
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn info_from_json(value: &Value) -> Option<UpstreamErrorInfo> {
    let detail = find_detail(value)?;
    let mut info = UpstreamErrorInfo::default();
    match detail {
        Value::String(text) => info.message = text.clone(),
        Value::Object(map) => {
            info.message = match map.get("message").or_else(|| map.get("detail")) {
                Some(Value::String(text)) => text.clone(),
                Some(other) if !other.is_null() => other.to_string(),
                _ => String::new(),
            };
            info.error_type = scalar_string(map.get("type"))
                // Google-shaped errors carry the class in `status`.
                .or_else(|| scalar_string(map.get("status")).filter(|s| s.parse::<u64>().is_err()));
            info.code = scalar_string(map.get("code"));
            info.retry_after_ms = explicit_retry_ms(detail);
        }
        _ => return None,
    }
    if info.message.trim().is_empty() {
        info.message = info
            .code
            .clone()
            .or_else(|| info.error_type.clone())
            .unwrap_or_else(|| value.to_string());
    }
    if info.retry_after_ms.is_none() {
        info.retry_after_ms = explicit_retry_ms(value).or_else(|| retry_hint_ms(&info.message));
    }
    info.message = sanitize_message(&info.message);
    Some(info)
}

/// Numeric retry fields some upstreams put next to the message.
fn explicit_retry_ms(value: &Value) -> Option<u64> {
    if let Some(ms) = u64_field(value, "retry_after_ms") {
        return Some(ms);
    }
    for key in ["retry_after", "retry_after_seconds", "resets_in_seconds"] {
        if let Some(seconds) = value.get(key).and_then(Value::as_f64)
            && seconds.is_finite()
            && seconds >= 0.0
        {
            return Some((seconds * 1000.0).ceil() as u64);
        }
    }
    None
}

fn looks_like_html(text: &str) -> bool {
    let head: String = text
        .chars()
        .take(256)
        .collect::<String>()
        .to_ascii_lowercase();
    head.starts_with("<!doctype") || head.starts_with("<html") || head.contains("<head")
}

fn html_title(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    // ASCII lower-casing keeps byte offsets, so the slice is valid in `text`.
    let title = text.get(start..end)?.trim();
    if title.is_empty() {
        None
    } else {
        Some(truncate_chars(title, 200))
    }
}

/// Finds a "try again in 1.5s" / "retry after 20 seconds" / "in 6m0s" hint
/// in an error message and returns it in milliseconds.
pub(crate) fn retry_hint_ms(message: &str) -> Option<u64> {
    let lower = message.to_ascii_lowercase();
    for marker in [
        "try again in ",
        "retry after ",
        "retry in ",
        "retry-after: ",
    ] {
        let mut search_from = 0;
        while let Some(pos) = lower[search_from..].find(marker) {
            let start = search_from + pos + marker.len();
            if let Some(ms) = parse_duration_ms(&lower[start..]) {
                return Some(ms);
            }
            search_from = start;
        }
    }
    None
}

/// Parses a leading duration such as `1.242s`, `120ms`, `6m0s`, `2 minutes`,
/// `20 seconds`. A bare number counts as seconds.
fn parse_duration_ms(text: &str) -> Option<u64> {
    let mut rest = text.trim_start();
    let mut total_ms = 0f64;
    let mut segments = 0;
    loop {
        let digits = rest
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_digit() || *c == '.')
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        if digits == 0 {
            break;
        }
        let Ok(number) = rest[..digits].trim_end_matches('.').parse::<f64>() else {
            break;
        };
        let after = rest[digits..].trim_start_matches(' ');
        let unit_len = after
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_alphabetic())
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        let factor = match &after[..unit_len] {
            "ms" | "msec" | "millisecond" | "milliseconds" => 1.0,
            "" | "s" | "sec" | "secs" | "second" | "seconds" => 1000.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
            _ if segments == 0 => return None,
            _ => break,
        };
        total_ms += number * factor;
        segments += 1;
        rest = &after[unit_len..];
        // Compound Go-style durations (`6m0s`) continue without a separator.
        if unit_len == 0 || !rest.starts_with(|c: char| c.is_ascii_digit()) {
            break;
        }
    }
    if segments == 0 || !total_ms.is_finite() {
        return None;
    }
    Some(total_ms.ceil() as u64)
}

// ---------------------------------------------------------------------------
// In-stream failures
// ---------------------------------------------------------------------------

/// Codes that mean "the request itself is at fault".
const REQUEST_FAULT_CODES: &[&str] = &[
    "invalid_prompt",
    "context_length_exceeded",
    "message_too_big",
    "string_above_max_length",
    "invalid_value",
    "unsupported_value",
    "invalid_request_error",
    "invalid_request",
    "bad_request_error",
    "previous_response_not_found",
    "cyber_policy",
    "bio_policy",
    "context_too_large",
];

/// Phrases that mark a context-overflow failure whatever code it carries.
const CONTEXT_OVERFLOW_PHRASES: &[&str] = &["context window", "context length", "too many tokens"];

fn status_in_range(value: Option<&Value>) -> Option<u16> {
    let n = value?.as_u64()?;
    if (400..=599).contains(&n) {
        Some(n as u16)
    } else {
        None
    }
}

/// HTTP status an in-stream failure stands for.
///
/// Explicit fields win, in this order: `status`, `status_code`,
/// `error.status`, `error.status_code`, `response.error.status`,
/// `response.error.status_code` (first one within 400..=599). Without one the
/// vendor's error code decides (`rate_limit_exceeded` → 429, request faults
/// → 400, …), then a message that describes a context overflow (400: the
/// request is at fault and no other credential would fare better); anything
/// unrecognised is a 502.
fn failure_status(payload: &Value, code: Option<&str>, kind: Option<&str>, message: &str) -> u16 {
    let error = payload.get("error");
    let nested = payload.get("response").and_then(|r| r.get("error"));
    let explicit = [
        payload.get("status"),
        payload.get("status_code"),
        error.and_then(|e| e.get("status")),
        error.and_then(|e| e.get("status_code")),
        nested.and_then(|e| e.get("status")),
        nested.and_then(|e| e.get("status_code")),
    ]
    .into_iter()
    .find_map(status_in_range);
    if let Some(status) = explicit {
        return status;
    }
    for label in [code, kind].into_iter().flatten() {
        let label = label.to_ascii_lowercase();
        let status = match label.as_str() {
            "rate_limit_exceeded"
            | "rate_limit_error"
            | "slow_down"
            | "insufficient_quota"
            | "usage_limit_reached"
            | "credit_balance_exhausted"
            // The account is out of money: what the HTTP API answers
            // with a 429 `insufficient_quota`-style error.
            | "billing_hard_limit_reached"
            | "billing_not_active"
            | "insufficient_balance"
            | "organization_spend_limit_exceeded"
            | "project_spend_limit_exceeded"
            | "enforced_spend_limit_reached" => Some(429),
            "invalid_api_key" | "authentication_error" | "unauthorized" => Some(401),
            "permission_error" | "permission_denied" | "forbidden" => Some(403),
            "model_not_found" | "not_found_error" | "not_found" => Some(404),
            "server_is_overloaded" | "service_unavailable_error" | "overloaded_error" => Some(503),
            "request_timeout" | "timeout" => Some(504),
            other if REQUEST_FAULT_CODES.contains(&other) => Some(400),
            _ => None,
        };
        if let Some(status) = status {
            return status;
        }
    }
    let message = message.to_ascii_lowercase();
    if CONTEXT_OVERFLOW_PHRASES
        .iter()
        .any(|phrase| message.contains(phrase))
    {
        return 400;
    }
    502
}

/// Converts an upstream failure payload — an `error` event (flat or nested),
/// a `response.failed` event, or any frame carrying an error object — into
/// the error handed to the client.
pub(crate) fn api_error_from_stream(payload: &Value) -> ApiError {
    let detail = payload
        .get("error")
        .filter(|v| v.is_object())
        .or_else(|| {
            payload
                .get("response")
                .and_then(|r| r.get("error"))
                .filter(|v| v.is_object())
        })
        .unwrap_or(payload);
    let message = non_empty(detail, "message")
        .map(str::to_string)
        .or_else(|| {
            payload
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "upstream reported an error without a message".to_string());
    let code = scalar_string(detail.get("code"));
    let kind = str_field(detail, "type").filter(|t| *t != "error");
    let status = failure_status(payload, code.as_deref(), kind, &message);
    let error_kind = match status {
        // A 500 here is the upstream's failure, not a gateway bug.
        500 => ErrorKind::Upstream,
        other => ErrorKind::from_status(other),
    };
    let mut error = ApiError::new(error_kind, sanitize_message(&message)).with_status(status);
    error.code = code.or_else(|| kind.map(|kind| sanitize(kind, MAX_FIELD_CHARS)));
    error.param = non_empty(detail, "param").map(|param| sanitize(param, MAX_FIELD_CHARS));
    error.retry_after_secs = explicit_retry_ms(detail)
        .or_else(|| retry_hint_ms(&message))
        .map(|ms| ms.div_ceil(1000).max(1));
    error
}

/// The canonical terminal `error` stream event.
///
/// OpenAI documents the SSE `error` event as flat (`code`, `message`,
/// `param`) and the WebSocket one as nested (`status` + `error{…}`). Clients
/// in the wild parse either, so the event carries both: the nested object
/// plus top-level copies. The same JSON is therefore valid as a WebSocket
/// frame.
pub(crate) fn stream_error_event(error: &ApiError, sequence_number: u64) -> Value {
    let detail = error_detail(error);
    let mut event = Map::new();
    event.insert("type".into(), json!("error"));
    event.insert("sequence_number".into(), json!(sequence_number));
    event.insert("status".into(), json!(error.status));
    event.insert("code".into(), detail["code"].clone());
    event.insert("message".into(), detail["message"].clone());
    event.insert("param".into(), detail["param"].clone());
    event.insert("error".into(), detail);
    Value::Object(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("1.242s."), Some(1242));
        assert_eq!(parse_duration_ms("120ms"), Some(120));
        assert_eq!(parse_duration_ms("6m0s"), Some(360_000));
        assert_eq!(parse_duration_ms("20 seconds"), Some(20_000));
        assert_eq!(parse_duration_ms("2 minutes and"), Some(120_000));
        assert_eq!(parse_duration_ms("1h2m3s"), Some(3_723_000));
        assert_eq!(parse_duration_ms("17"), Some(17_000));
        assert_eq!(parse_duration_ms("a while"), None);
        assert_eq!(parse_duration_ms("3 tries"), None);
    }

    #[test]
    fn hints_in_messages() {
        assert_eq!(
            retry_hint_ms("Rate limit reached. Please try again in 3.2s. Visit …"),
            Some(3200)
        );
        assert_eq!(retry_hint_ms("Please retry after 20 seconds"), Some(20_000));
        assert_eq!(
            retry_hint_ms("try again in a moment; try again in 5s"),
            Some(5000)
        );
        assert_eq!(retry_hint_ms("no hint here"), None);
    }

    const KEY: &str = "sk-live-9f8e7d6c5b4a3f2e1d0c";

    /// Redaction slices text at computed offsets; a deterministic walk over
    /// strings assembled from the fragments it looks for (and multi-byte
    /// characters next to them) checks that it never panics, reaches a fixed
    /// point after one pass, and never leaves a key behind that stands on
    /// its own (one glued to the end of a word is indistinguishable from a
    /// hyphenated identifier such as `task-…` and is left to the other two
    /// rules).
    #[test]
    fn redaction_never_panics_and_is_idempotent_on_assembled_text() {
        const FRAGMENTS: &[&str] = &[
            "Bearer",
            "bearer ",
            "Basic ",
            "authorization",
            "api_key",
            "api-key",
            "token",
            "secret",
            "password",
            "key",
            "sk-",
            "=",
            ":",
            ": ",
            " = ",
            "\"",
            "'",
            "\\",
            "?",
            "&",
            ",",
            ";",
            "}",
            "{",
            " ",
            "\n",
            "\t",
            ".",
            "é",
            "末",
            "🔑",
            "[REDACTED]",
            "abc",
            "x1",
            "tokens",
            KEY,
        ];
        let mut state: u64 = 0x1234_5678_9ABC_DEF1;
        let mut next = |bound: usize| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as usize % bound
        };
        for _ in 0..20_000 {
            let text: String = (0..1 + next(10))
                .map(|_| FRAGMENTS[next(FRAGMENTS.len())])
                .collect();
            let once = redact_secrets(&text);
            assert_eq!(
                redact_secrets(&once),
                once,
                "not a fixed point for {text:?}"
            );
            let glued = once
                .match_indices(KEY)
                .all(|(at, _)| at > 0 && is_word_byte(once.as_bytes()[at - 1]));
            assert!(glued, "{text:?} -> {once:?}");
        }
    }

    #[test]
    fn redacts_bearer_tokens_but_not_prose() {
        assert_eq!(
            redact_secrets(&format!("header Authorization: Bearer {KEY} is not valid")),
            "header Authorization: Bearer [REDACTED] is not valid"
        );
        assert_eq!(
            redact_secrets("got bearer  abc.def-123. Try again"),
            "got bearer  [REDACTED]. Try again"
        );
        // The vendor's own guidance for a missing key stays readable.
        let guidance = "You need to provide your API key in an Authorization header using Bearer auth (i.e. Authorization: Bearer YOUR_KEY).";
        assert_eq!(
            redact_secrets(guidance),
            "You need to provide your API key in an Authorization header using Bearer auth (i.e. Authorization: Bearer [REDACTED])."
        );
        assert_eq!(
            redact_secrets("Bearer sk_live_abc and Bearer eyJhbGciOi.J9-_x"),
            "Bearer [REDACTED] and Bearer [REDACTED]"
        );
        assert_eq!(
            redact_secrets("a bearer token is required"),
            "a bearer token is required"
        );
        assert_eq!(redact_secrets("forbearer x1"), "forbearer x1");
    }

    #[test]
    fn redacts_values_assigned_to_secret_names() {
        assert_eq!(
            redact_secrets(&format!("backend call failed (api_key={KEY})")),
            "backend call failed (api_key=[REDACTED]"
        );
        assert_eq!(
            redact_secrets("token=abc123 was rejected"),
            "token=[REDACTED] was rejected"
        );
        assert_eq!(
            redact_secrets("x-api-key: abc; access_token = 'q r'"),
            "x-api-key: [REDACTED]; access_token = '[REDACTED]'"
        );
        assert_eq!(
            redact_secrets(r#"{"authorization":"Bearer abc def","x":"y"}"#),
            r#"{"authorization":"[REDACTED]","x":"y"}"#
        );
        assert_eq!(
            redact_secrets(r#"{"client_secret":"s3cr3t","password": "hunter2","n":1}"#),
            r#"{"client_secret":"[REDACTED]","password": "[REDACTED]","n":1}"#
        );
        assert_eq!(
            redact_secrets("GET https://host/v1/models?key=AIzaSyA-1&alt=sse failed"),
            "GET https://host/v1/models?key=[REDACTED]&alt=sse failed"
        );
        assert_eq!(
            redact_secrets("Authorization: Basic dXNlcjpwYXNz, next"),
            "Authorization: Basic [REDACTED], next"
        );
    }

    #[test]
    fn redaction_leaves_token_counts_and_ordinary_text_alone() {
        for text in [
            "max_output_tokens: 16 is below the minimum",
            "Rate limit reached for gpt-5 on tokens per min (TPM): Limit 30000, Used 29000.",
            "token_count: 5, input_tokens=12, \"total_tokens\":48",
            "Incorrect API key provided: sk-proj-********************abcd. See https://platform.openai.com/account/api-keys.",
            "The monkey: 12; a key= value",
            "naïve text with ünïcödé and no secrets",
            "",
        ] {
            assert_eq!(redact_secrets(text), text);
        }
    }

    #[test]
    fn redacts_bare_vendor_keys_and_is_idempotent() {
        let leaked = format!("Incorrect API key provided: {KEY}. Check it.");
        let clean = redact_secrets(&leaked);
        assert_eq!(clean, "Incorrect API key provided: [REDACTED]. Check it.");
        for text in [
            clean.as_str(),
            "Authorization: Bearer [REDACTED] is not valid",
            "api_key=[REDACTED]) token: [REDACTED]",
        ] {
            assert_eq!(redact_secrets(text), text);
        }
        // Not a key: too short, or part of a longer word.
        assert_eq!(redact_secrets("task-1234 sk-short"), "task-1234 sk-short");
    }

    #[test]
    fn redaction_survives_multibyte_text_around_secrets() {
        assert_eq!(
            redact_secrets("clé: token=é√∆abc défaut"),
            "clé: token=[REDACTED] défaut"
        );
        assert_eq!(redact_secrets("Bearer é"), "Bearer é");
        assert_eq!(redact_secrets("secret:"), "secret:");
        assert_eq!(
            redact_secrets("authorization: bearer"),
            "authorization: bearer"
        );
    }

    #[test]
    fn html_titles() {
        assert_eq!(
            html_title("<html><head><TITLE> 502 Bad Gateway </TITLE></head></html>").as_deref(),
            Some("502 Bad Gateway")
        );
        assert_eq!(html_title("<html><body>x</body></html>"), None);
    }
}
