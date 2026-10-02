//! The Messages error envelope: rendering [`ApiError`] for clients and
//! understanding upstream error bodies.
//!
//! Error text that comes from an upstream is never trusted to be free of
//! credentials (see [`crate::redact`]): everything [`parse_error_value`] and
//! [`decode_error`] return is redacted, and so is every message that leaves
//! through [`encode_error`].

use crate::redact::redact_secrets;
use crate::util::str_field;
use serde_json::{Value, json};
use switchyard_core::util::truncate_chars;
use switchyard_core::{ApiError, ErrorKind, UpstreamErrorInfo};

/// Longest message kept from a body that is not a recognised error envelope.
const MAX_RAW_MESSAGE_CHARS: usize = 600;

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// The Messages `error.type` for an error. The kind decides, except for the
/// statuses whose type the kind cannot tell: 402, 409 and 529 have no
/// [`ErrorKind`], and a 413 relayed from an upstream arrives classified as
/// a plain request fault ([`ErrorKind::InvalidRequest`]) while the API's
/// type for that status is `request_too_large`.
pub(crate) fn error_type(error: &ApiError) -> &'static str {
    match error.status {
        402 => return "billing_error",
        409 => return "conflict_error",
        413 => return "request_too_large",
        529 => return "overloaded_error",
        _ => {}
    }
    match error.kind {
        ErrorKind::InvalidRequest => "invalid_request_error",
        ErrorKind::Authentication => "authentication_error",
        ErrorKind::Permission => "permission_error",
        ErrorKind::NotFound => "not_found_error",
        ErrorKind::TooLarge => "request_too_large",
        ErrorKind::RateLimit => "rate_limit_error",
        ErrorKind::Timeout => "timeout_error",
        // "No capacity right now" is what the vendor calls overloaded.
        ErrorKind::Unavailable => "overloaded_error",
        ErrorKind::Upstream | ErrorKind::Internal => "api_error",
    }
}

/// `{"type":"error","error":{"type":…,"message":…}}`. The same object is the
/// payload of an in-stream `event: error`.
///
/// The message is redacted once more on the way out: an error another
/// protocol's decoder took from its upstream is rendered here too, and this
/// is the last place to catch a credential before it reaches a client.
pub(crate) fn encode_error(error: &ApiError) -> Value {
    json!({
        "type": "error",
        "error": {"type": error_type(error), "message": redact_secrets(&error.message)},
    })
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// A non-empty string, or a number rendered as text.
fn scalar_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// The human readable part of an error object, whatever the vendor calls it.
fn message_text(object: &Value) -> Option<String> {
    for key in [
        "message",
        "msg",
        "detail",
        "error_description",
        "error_message",
    ] {
        match object.get(key) {
            Some(Value::String(text)) if !text.trim().is_empty() => return Some(text.clone()),
            // FastAPI-style structured details.
            Some(detail @ (Value::Array(_) | Value::Object(_))) if key == "detail" => {
                return Some(detail.to_string());
            }
            _ => {}
        }
    }
    None
}

/// A retry delay stated as a JSON value: a number of seconds, or a duration
/// string such as `"30s"`.
fn delay_value_ms(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number
            .as_f64()
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .map(|seconds| (seconds * 1000.0).round() as u64),
        Value::String(text) => parse_duration_ms(text),
        _ => None,
    }
}

/// Retry hints carried as fields of an error object.
fn retry_fields_ms(object: &Value) -> Option<u64> {
    if let Some(ms) = object
        .get("retry_after_ms")
        .and_then(Value::as_f64)
        .filter(|ms| ms.is_finite() && *ms >= 0.0)
    {
        return Some(ms.round() as u64);
    }
    for key in ["retry_after", "retryAfter", "retry_delay", "retryDelay"] {
        if let Some(ms) = object.get(key).and_then(delay_value_ms) {
            return Some(ms);
        }
    }
    // Google `RetryInfo` detail, relayed verbatim by some gateways.
    object
        .get("details")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|detail| detail.get("retryDelay").and_then(delay_value_ms))
}

/// Parses durations such as `3.2s`, `250ms`, `1m30s`, `2 minutes`, `45`
/// (seconds) from the start of `text`. Returns milliseconds.
pub(crate) fn parse_duration_ms(text: &str) -> Option<u64> {
    let mut rest = text.trim_start();
    let mut total_ms = 0f64;
    let mut pieces = 0;
    loop {
        let digits = rest
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_digit() || *c == '.')
            .last()
            .map(|(at, c)| at + c.len_utf8())
            .unwrap_or(0);
        if digits == 0 {
            break;
        }
        let Ok(number) = rest[..digits].trim_end_matches('.').parse::<f64>() else {
            break;
        };
        // A trailing sentence dot is not part of the number.
        let consumed = rest[..digits].trim_end_matches('.').len();
        rest = rest[consumed..].trim_start_matches(' ');
        let unit_len = rest
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_alphabetic())
            .last()
            .map(|(at, c)| at + c.len_utf8())
            .unwrap_or(0);
        let unit = rest[..unit_len].to_ascii_lowercase();
        let factor = match unit.as_str() {
            "ms" | "msec" | "msecs" | "millisecond" | "milliseconds" => 1.0,
            "" | "s" | "sec" | "secs" | "second" | "seconds" => 1000.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
            // A word that is not a unit ends the duration; the bare number
            // before it counts as seconds.
            _ => {
                total_ms += number * 1000.0;
                pieces += 1;
                break;
            }
        };
        total_ms += number * factor;
        pieces += 1;
        rest = &rest[unit_len..];
        if unit.is_empty() {
            break;
        }
    }
    if pieces == 0 || !total_ms.is_finite() {
        return None;
    }
    Some(total_ms.round() as u64)
}

/// Finds "try again in 3.2s"-style hints in an error message.
pub(crate) fn retry_hint_in_text(text: &str) -> Option<u64> {
    let lower = text.to_ascii_lowercase();
    for marker in [
        "try again in ",
        "retry after ",
        "retry in ",
        "retry-after: ",
        "please wait ",
    ] {
        if let Some(at) = lower.find(marker)
            && let Some(ms) = parse_duration_ms(&lower[at + marker.len()..])
        {
            return Some(ms);
        }
    }
    None
}

/// Extracts message, type and code from a JSON error body. Understands
///
/// * Anthropic: `{"type":"error","error":{"type","message"},"request_id"}`;
/// * OpenAI-shaped bodies of compatible gateways:
///   `{"error":{"message","type","code","param"}}`;
/// * Google-shaped: `{"error":{"code":429,"message","status"}}`, possibly
///   wrapped in an array;
/// * flat bodies: `{"error":"…"}`, `{"message":"…"}`, `{"detail":"…"}`.
///
/// `None` when the value has none of these shapes. The text fields of the
/// result are free of credentials.
pub(crate) fn parse_error_value(value: &Value) -> Option<UpstreamErrorInfo> {
    let mut info = parse_error_fields(value)?;
    info.message = redact_secrets(&info.message);
    info.error_type = info.error_type.map(|kind| redact_secrets(&kind));
    info.code = info.code.map(|code| redact_secrets(&code));
    Some(info)
}

fn parse_error_fields(value: &Value) -> Option<UpstreamErrorInfo> {
    let value = match value {
        Value::Array(items) => items.first()?,
        Value::String(text) if !text.trim().is_empty() => {
            return Some(UpstreamErrorInfo {
                message: text.clone(),
                ..UpstreamErrorInfo::default()
            });
        }
        other => other,
    };
    if !value.is_object() {
        return None;
    }
    let top_type = str_field(value, "type")
        .filter(|kind| !kind.is_empty() && *kind != "error")
        .map(str::to_string);
    let mut info = UpstreamErrorInfo::default();
    match value.get("error") {
        Some(error @ Value::Object(_)) => {
            info.error_type = str_field(error, "type")
                .or_else(|| str_field(error, "status"))
                .filter(|kind| !kind.is_empty())
                .map(str::to_string)
                .or(top_type);
            info.code = scalar_text(error.get("code")).or_else(|| {
                // Anthropic puts machine readable sub-codes here, e.g.
                // `enforced_spend_limit_reached` on a monthly spend cap.
                error
                    .get("details")
                    .filter(|details| details.is_object())
                    .and_then(|details| scalar_text(details.get("error_code")))
            });
            info.message = message_text(error).unwrap_or_default();
            info.retry_after_ms = retry_fields_ms(error).or_else(|| retry_fields_ms(value));
        }
        Some(Value::String(text)) if !text.trim().is_empty() => {
            // `{"error":"invalid_grant","error_description":"…"}` and
            // `{"error":"message"}` both exist.
            match message_text(value) {
                Some(message) => {
                    info.message = message;
                    info.code = Some(text.clone());
                }
                None => info.message = text.clone(),
            }
            info.error_type = top_type;
            if info.code.is_none() {
                info.code = scalar_text(value.get("code"));
            }
            info.retry_after_ms = retry_fields_ms(value);
        }
        _ => {
            info.message = message_text(value).unwrap_or_default();
            info.error_type = top_type;
            info.code = scalar_text(value.get("code"));
            info.retry_after_ms = retry_fields_ms(value);
        }
    }
    if info.code.is_some() && info.code == info.error_type {
        info.code = None;
    }
    if info.message.is_empty() {
        // An envelope with a type but no message still says something.
        info.message = info.error_type.clone().or_else(|| info.code.clone())?;
    }
    if info.retry_after_ms.is_none() {
        info.retry_after_ms = retry_hint_in_text(&info.message);
    }
    Some(info)
}

fn looks_like_html(text: &str) -> bool {
    let head: String = text
        .chars()
        .take(256)
        .collect::<String>()
        .to_ascii_lowercase();
    head.starts_with("<!doctype")
        || head.starts_with("<html")
        || head.contains("<head")
        || head.contains("<body")
}

/// The `<title>` of an HTML page, when it has one.
fn html_title(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    let title = text
        .get(start..end)?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!title.is_empty()).then_some(title)
}

/// The JSON payload of the first `data:` line, for upstreams that answer an
/// error status with an SSE-framed body.
fn sse_payload(text: &str) -> Option<Value> {
    if !(text.starts_with("event:") || text.starts_with("data:")) {
        return None;
    }
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .find_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
}

/// Extracts what it can from an upstream error body. Never fails:
///
/// * JSON envelopes are parsed with [`parse_error_value`];
/// * an HTML page (proxy and CDN errors) is summarised by its title;
/// * any other text is used as the message, truncated;
/// * an empty body yields a message naming the status.
///
/// Retry hints are taken from body fields first and from the message text
/// ("try again in 3.2s") otherwise.
pub(crate) fn decode_error(status: u16, body: &[u8]) -> UpstreamErrorInfo {
    let text = String::from_utf8_lossy(body);
    let text = text.trim().trim_start_matches('\u{feff}');
    if text.is_empty() {
        return UpstreamErrorInfo {
            message: format!("upstream returned HTTP {status} with an empty body"),
            ..UpstreamErrorInfo::default()
        };
    }
    let parsed = serde_json::from_str::<Value>(text)
        .ok()
        .or_else(|| sse_payload(text));
    if let Some(info) = parsed.as_ref().and_then(parse_error_value) {
        return UpstreamErrorInfo {
            message: truncate_chars(&info.message, MAX_RAW_MESSAGE_CHARS * 4),
            ..info
        };
    }
    let message = if looks_like_html(text) {
        match html_title(text) {
            Some(title) => format!(
                "upstream returned an HTML page (HTTP {status}): {}",
                truncate_chars(&redact_secrets(&title), 200)
            ),
            None => format!("upstream returned an HTML page (HTTP {status})"),
        }
    } else {
        truncate_chars(&redact_secrets(text), MAX_RAW_MESSAGE_CHARS)
    };
    UpstreamErrorInfo {
        retry_after_ms: retry_hint_in_text(text),
        message,
        error_type: None,
        code: None,
    }
}

/// Converts the payload of an in-stream `error` event into the error the
/// client is told. Statuses follow the vendor's table with two adjustments:
/// `overloaded_error` (529) is the canonical 503, and credential problems of
/// the *upstream* are a bad gateway, not the client's fault.
pub(crate) fn stream_error(payload: &Value) -> ApiError {
    let info = parse_error_value(payload).unwrap_or_else(|| UpstreamErrorInfo {
        message: "the upstream reported an error mid-stream".to_string(),
        ..UpstreamErrorInfo::default()
    });
    let kind = match info.error_type.as_deref().unwrap_or("") {
        "overloaded_error" => ErrorKind::Unavailable,
        "rate_limit_error" | "rate_limit_exceeded" | "insufficient_quota" => ErrorKind::RateLimit,
        "invalid_request_error" => ErrorKind::InvalidRequest,
        "not_found_error" => ErrorKind::NotFound,
        "request_too_large" => ErrorKind::TooLarge,
        "timeout_error" => ErrorKind::Timeout,
        // authentication_error, permission_error, billing_error, api_error
        // and anything newer.
        _ => ErrorKind::Upstream,
    };
    let mut error = ApiError::new(kind, info.message);
    error.code = info.error_type.or(info.code);
    if let Some(ms) = info.retry_after_ms {
        error.retry_after_secs = Some(ms.div_ceil(1000).max(1));
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("3.2s"), Some(3200));
        assert_eq!(parse_duration_ms("250ms."), Some(250));
        assert_eq!(parse_duration_ms("1m30s"), Some(90_000));
        assert_eq!(parse_duration_ms("6m0s."), Some(360_000));
        assert_eq!(parse_duration_ms("2 minutes"), Some(120_000));
        assert_eq!(
            parse_duration_ms("30 seconds before retrying"),
            Some(30_000)
        );
        assert_eq!(parse_duration_ms("45"), Some(45_000));
        assert_eq!(parse_duration_ms("20."), Some(20_000));
        assert_eq!(parse_duration_ms("1h"), Some(3_600_000));
        assert_eq!(parse_duration_ms("soon"), None);
        assert_eq!(parse_duration_ms(""), None);
    }

    #[test]
    fn text_hints() {
        assert_eq!(
            retry_hint_in_text("Rate limit reached. Please try again in 1.5s."),
            Some(1500)
        );
        assert_eq!(retry_hint_in_text("Retry after 20 seconds"), Some(20_000));
        assert_eq!(retry_hint_in_text("Overloaded"), None);
    }

    #[test]
    fn html_titles() {
        assert_eq!(
            html_title("<html><head><title>\n 502 Bad\n Gateway </title></head></html>").as_deref(),
            Some("502 Bad Gateway")
        );
        assert_eq!(html_title("<html><body>x</body></html>"), None);
    }
}
