//! Error envelopes: rendering [`ApiError`] for Chat clients, parsing the
//! error bodies of OpenAI and of the many servers that imitate it, and
//! turning in-stream error frames into [`ApiError`]s.
//!
//! Error text that comes from an upstream is never trusted to be free of
//! credentials (see [`crate::redact`]): everything that enters through
//! [`decode_error`] / [`api_error_from_stream`] is redacted, and so is every
//! message that leaves through [`encode_error`].

use crate::redact::redact_secrets;
use serde_json::{Value, json};
use switchyard_core::util::truncate_chars;
use switchyard_core::{ApiError, ErrorKind, UpstreamErrorInfo};

/// Longest message kept from an upstream error body.
const MAX_MESSAGE_CHARS: usize = 2000;

/// OpenAI `error.type` and the default `error.code` for an error kind.
///
/// | kind | `type` | default `code` |
/// |---|---|---|
/// | `InvalidRequest` | `invalid_request_error` | – |
/// | `Authentication` | `authentication_error` | `invalid_api_key` |
/// | `Permission` | `permission_error` | – |
/// | `NotFound` | `invalid_request_error` | `not_found` |
/// | `TooLarge` | `invalid_request_error` | `request_too_large` |
/// | `RateLimit` | `rate_limit_error` | `rate_limit_exceeded` |
/// | `Upstream` | `server_error` | `upstream_error` |
/// | `Unavailable` | `service_unavailable_error` | `service_unavailable` |
/// | `Timeout` | `server_error` | `request_timeout` |
/// | `Internal` | `server_error` | `internal_server_error` |
///
/// `Unavailable` has a type of its own because OpenAI's has one: its 503
/// bodies say `service_unavailable_error`, and clients (and gateways stacked
/// on this one) tell "overloaded, try again later" from a generic
/// `server_error` by it. The Responses codec renders the same type.
fn type_and_code(kind: ErrorKind) -> (&'static str, Option<&'static str>) {
    match kind {
        ErrorKind::InvalidRequest => ("invalid_request_error", None),
        ErrorKind::Authentication => ("authentication_error", Some("invalid_api_key")),
        ErrorKind::Permission => ("permission_error", None),
        ErrorKind::NotFound => ("invalid_request_error", Some("not_found")),
        ErrorKind::TooLarge => ("invalid_request_error", Some("request_too_large")),
        ErrorKind::RateLimit => ("rate_limit_error", Some("rate_limit_exceeded")),
        ErrorKind::Upstream => ("server_error", Some("upstream_error")),
        ErrorKind::Unavailable => ("service_unavailable_error", Some("service_unavailable")),
        ErrorKind::Timeout => ("server_error", Some("request_timeout")),
        ErrorKind::Internal => ("server_error", Some("internal_server_error")),
    }
}

/// Renders an error as the OpenAI error body
/// `{"error":{"message","type","param","code"}}`. The same object is the
/// payload of an in-stream error frame.
///
/// The message is redacted once more on the way out: an error another
/// protocol's decoder took from its upstream is rendered here too, and this
/// is the last place to catch a credential before it reaches a client.
pub(crate) fn encode_error(error: &ApiError) -> Value {
    let (kind, default_code) = type_and_code(error.kind);
    json!({
        "error": {
            "message": redact_secrets(&error.message),
            "type": kind,
            "param": error.param,
            "code": error.code.as_deref().or(default_code),
        }
    })
}

// ---------------------------------------------------------------------------
// Upstream error bodies
// ---------------------------------------------------------------------------

/// Parses an upstream error body. Never fails: a body that is not JSON (an
/// HTML error page from a proxy, plain text, nothing at all) is summarised.
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
            Some(title) => clean_message(&format!("upstream returned HTTP {status}: {title}")),
            None => format!("upstream returned HTTP {status} with an HTML error page"),
        }
    } else {
        clean_message(trimmed)
    };
    UpstreamErrorInfo {
        message,
        error_type: None,
        code: None,
        retry_after_ms: retry_hint_ms(trimmed),
    }
}

/// Upstream error text as it may be shown: credentials removed, then cut to
/// the longest message kept.
pub(crate) fn clean_message(text: &str) -> String {
    truncate_chars(&redact_secrets(text), MAX_MESSAGE_CHARS)
}

/// Locates the error object inside the shapes "OpenAI-compatible" servers
/// use: `{"error":{…}}`, `{"error":"text"}`, a bare `{"message":…}` /
/// `{"detail":…}` (FastAPI, vLLM), or Google's one-element array wrapper.
fn find_detail(value: &Value) -> Option<&Value> {
    match value {
        Value::Array(items) => items.first().and_then(find_detail),
        Value::Object(map) => {
            if let Some(error) = map.get("error").filter(|v| !v.is_null()) {
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

fn scalar_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) if !text.trim().is_empty() => Some(redact_secrets(text.trim())),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn message_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        // FastAPI validation errors put a list of objects under `detail`.
        other => other.to_string(),
    }
}

fn info_from_json(value: &Value) -> Option<UpstreamErrorInfo> {
    let detail = find_detail(value)?;
    let mut info = UpstreamErrorInfo::default();
    match detail {
        Value::String(text) => {
            info.message = text.clone();
            // `{"error":"…","code":…,"type":…}` keeps the labels next to it.
            info.error_type = scalar_string(value.get("type"));
            info.code = scalar_string(value.get("code"));
        }
        Value::Object(map) => {
            info.message = map
                .get("message")
                .or_else(|| map.get("detail"))
                .map(message_text)
                .unwrap_or_default();
            info.error_type = scalar_string(map.get("type"))
                // Google-shaped errors carry the class in `status`.
                .or_else(|| scalar_string(map.get("status")).filter(|s| s.parse::<u64>().is_err()));
            info.code = scalar_string(map.get("code"));
            // OpenRouter relays the provider's own body under `metadata.raw`;
            // it is what tells a quota error from a rate limit.
            if let Some(raw) = map
                .get("metadata")
                .and_then(|m| m.get("raw"))
                .map(message_text)
                .filter(|raw| !raw.trim().is_empty() && !info.message.contains(raw.trim()))
            {
                info.message = format!("{} ({})", info.message.trim(), raw.trim());
            }
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
    info.message = clean_message(&info.message);
    Some(info)
}

fn seconds_to_ms(seconds: f64) -> Option<u64> {
    (seconds.is_finite() && seconds >= 0.0).then(|| (seconds * 1000.0).ceil() as u64)
}

/// Retry fields some upstreams put next to the message: numeric
/// `retry_after_ms` / `retry_after` (seconds), and Google's
/// `details[].retryDelay` duration string.
fn explicit_retry_ms(value: &Value) -> Option<u64> {
    if let Some(ms) = value
        .get("retry_after_ms")
        .and_then(Value::as_f64)
        .filter(|ms| ms.is_finite() && *ms >= 0.0)
    {
        return Some(ms.ceil() as u64);
    }
    for key in ["retry_after", "retry_after_seconds", "resets_in_seconds"] {
        if let Some(ms) = value
            .get(key)
            .and_then(Value::as_f64)
            .and_then(seconds_to_ms)
        {
            return Some(ms);
        }
    }
    value
        .get("details")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|d| d.get("retryDelay").or_else(|| d.get("retry_delay")))
        .filter_map(Value::as_str)
        .find_map(|delay| parse_duration_ms(&delay.to_ascii_lowercase()))
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
    // ASCII lower-casing keeps byte offsets, so the range is valid in `text`.
    let title = text.get(start..end)?.trim();
    (!title.is_empty()).then(|| truncate_chars(title, 200))
}

/// Finds a "try again in 1.242s" / "retry after 20 seconds" / "in 6m0s" hint
/// in an error message and returns it in milliseconds.
pub(crate) fn retry_hint_ms(message: &str) -> Option<u64> {
    let lower = message.to_ascii_lowercase();
    for marker in [
        "try again in ",
        "retry after ",
        "retry in ",
        "retry-after: ",
    ] {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(marker) {
            let start = from + pos + marker.len();
            if let Some(ms) = parse_duration_ms(&lower[start..]) {
                return Some(ms);
            }
            from = start;
        }
    }
    None
}

/// Parses a leading duration such as `1.242s`, `120ms`, `6m0s`, `2 minutes`
/// or `20 seconds` (lower-case input). A bare number counts as seconds.
fn parse_duration_ms(text: &str) -> Option<u64> {
    let mut rest = text.trim_start();
    let mut total_ms = 0f64;
    let mut segments = 0;
    loop {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let Ok(number) = rest[..digits].trim_end_matches('.').parse::<f64>() else {
            break;
        };
        let after = rest[digits..].trim_start_matches(' ');
        let unit_len = after
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(after.len());
        let factor = match &after[..unit_len] {
            "ms" | "msec" | "millisecond" | "milliseconds" => 1.0,
            "s" | "sec" | "secs" | "second" | "seconds" => 1000.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
            // A bare number is seconds; so is a number followed by prose.
            _ if segments == 0 => {
                total_ms = number * 1000.0;
                segments = 1;
                break;
            }
            _ => break,
        };
        total_ms += number * factor;
        segments += 1;
        rest = &after[unit_len..];
        // Go-style compound durations (`6m0s`) continue without a separator.
        if !rest.starts_with(|c: char| c.is_ascii_digit()) {
            break;
        }
    }
    (segments > 0 && total_ms.is_finite()).then(|| total_ms.ceil() as u64)
}

// ---------------------------------------------------------------------------
// In-stream failures
// ---------------------------------------------------------------------------

fn status_in_range(value: Option<&Value>) -> Option<u16> {
    let n = value?.as_u64()?;
    (400..=599).contains(&n).then_some(n as u16)
}

/// HTTP status an in-stream error stands for: an explicit numeric status
/// (`error.code` as OpenRouter sends it, `error.status`, top-level `status` /
/// `status_code`) or, failing that, what the vendor's type/code strings imply.
fn stream_status(payload: &Value, info: &UpstreamErrorInfo) -> u16 {
    let error = payload.get("error");
    let explicit = [
        error.and_then(|e| e.get("code")),
        error.and_then(|e| e.get("status")),
        error.and_then(|e| e.get("status_code")),
        payload.get("status"),
        payload.get("status_code"),
        payload.get("code"),
    ]
    .into_iter()
    .find_map(status_in_range);
    if let Some(status) = explicit {
        return status;
    }
    for label in [info.code.as_deref(), info.error_type.as_deref()]
        .into_iter()
        .flatten()
    {
        let label = label.to_ascii_lowercase();
        let status = match label.as_str() {
            "rate_limit_exceeded"
            | "rate_limit_error"
            | "requests"
            | "tokens"
            | "insufficient_quota"
            | "slow_down"
            | "resource_exhausted" => 429,
            "invalid_api_key" | "authentication_error" | "unauthenticated" => 401,
            "permission_error" | "permission_denied" => 403,
            "model_not_found" | "not_found_error" | "not_found" => 404,
            "server_is_overloaded"
            | "overloaded_error"
            | "service_unavailable_error"
            | "unavailable"
            | "engine_overloaded" => 503,
            "request_timeout" | "timeout" | "timeout_error" => 504,
            "invalid_request_error"
            | "invalid_request"
            | "bad_request_error"
            | "context_length_exceeded"
            | "string_above_max_length"
            | "invalid_prompt"
            | "content_policy_violation"
            | "invalid_argument" => 400,
            _ => continue,
        };
        return status;
    }
    502
}

/// Converts an in-stream error frame (`data: {"error":{…}}`, or the payload
/// of an SSE `error` event) into the error handed to the client.
///
/// Upstream authentication and permission failures are reported as 502: the
/// client's own key is fine, the gateway's upstream credential is not. An
/// exhausted upstream balance (402) is a 429, as for failed HTTP calls.
pub(crate) fn api_error_from_stream(payload: &Value) -> ApiError {
    let info = info_from_json(payload).unwrap_or_else(|| UpstreamErrorInfo {
        message: clean_message(&payload.to_string()),
        ..UpstreamErrorInfo::default()
    });
    let status = stream_status(payload, &info);
    let (kind, status) = match status {
        401 | 403 => (ErrorKind::Upstream, 502),
        402 => (ErrorKind::RateLimit, 429),
        503 | 529 => (ErrorKind::Unavailable, 503),
        408 | 504 => (ErrorKind::Timeout, 504),
        s if s >= 500 => (ErrorKind::Upstream, 502),
        s => (ErrorKind::from_status(s), s),
    };
    let mut error = ApiError::new(kind, info.message).with_status(status);
    // A numeric "code" is the status again, not a machine-readable code.
    error.code = info
        .code
        .filter(|c| c.parse::<u64>().is_err())
        .or(info.error_type);
    error.retry_after_secs = info.retry_after_ms.map(|ms| ms.div_ceil(1000).max(1));
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("1.242s."), Some(1242));
        assert_eq!(parse_duration_ms("120ms"), Some(120));
        assert_eq!(parse_duration_ms("6m0s"), Some(360_000));
        assert_eq!(parse_duration_ms("1h2m3s"), Some(3_723_000));
        assert_eq!(parse_duration_ms("2 minutes"), Some(120_000));
        assert_eq!(parse_duration_ms("20 seconds."), Some(20_000));
        assert_eq!(parse_duration_ms("34"), Some(34_000));
        assert_eq!(parse_duration_ms("7 or so"), Some(7_000));
        assert_eq!(parse_duration_ms("a while"), None);
        assert_eq!(parse_duration_ms(""), None);
    }

    #[test]
    fn retry_hints_skip_markers_without_a_duration() {
        assert_eq!(
            retry_hint_ms("Please try again in a moment, or retry after 3s."),
            Some(3000)
        );
        assert_eq!(
            retry_hint_ms("Rate limit reached. Please try again in 1.5s."),
            Some(1500)
        );
        assert_eq!(retry_hint_ms("nothing useful here"), None);
    }

    #[test]
    fn html_titles() {
        assert!(looks_like_html(
            "<!DOCTYPE html><html><head><title> 502 Bad Gateway </title>"
        ));
        assert_eq!(
            html_title("<html><head><TITLE>502 Bad Gateway</TITLE></head>").as_deref(),
            Some("502 Bad Gateway")
        );
        assert_eq!(html_title("<html><body>no title</body></html>"), None);
        assert!(!looks_like_html("upstream connect error"));
    }
}
