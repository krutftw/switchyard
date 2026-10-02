//! Google's error envelope: rendering [`ApiError`] for Gemini clients and
//! parsing upstream error bodies.
//!
//! ```json
//! {"error": {"code": 429, "message": "…", "status": "RESOURCE_EXHAUSTED",
//!            "details": [{"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "12s"}]}}
//! ```
//!
//! Google states retry hints in the body, not in headers: a `RetryInfo`
//! detail, an `ErrorInfo` `quotaResetDelay`, or a "Please retry in 12.3s."
//! sentence at the end of the message.
//!
//! Error text that comes from an upstream is never trusted to be free of
//! credentials (see [`crate::redact`]): everything that enters through
//! [`decode_error`] / [`api_error_from_payload`] is redacted, and so is
//! every message that leaves through [`encode_error`].

use crate::redact::redact_secrets;
use crate::util::pick_str;
use serde_json::{Map, Value, json};
use switchyard_core::util::truncate_chars;
use switchyard_core::{ApiError, ErrorKind, UpstreamErrorInfo};

/// Longest message kept from an upstream error body.
const MAX_MESSAGE_CHARS: usize = 2000;

/// `UpstreamErrorInfo::code` reported when a `RESOURCE_EXHAUSTED` error is a
/// per-day quota running out (retrying soon is pointless), as opposed to
/// per-minute throttling.
pub const CODE_DAILY_QUOTA: &str = "daily_quota_exceeded";

const RETRY_INFO: &str = "type.googleapis.com/google.rpc.RetryInfo";
const ERROR_INFO: &str = "type.googleapis.com/google.rpc.ErrorInfo";
const QUOTA_FAILURE: &str = "type.googleapis.com/google.rpc.QuotaFailure";
const BAD_REQUEST: &str = "type.googleapis.com/google.rpc.BadRequest";

/// The canonical `google.rpc.Code` name for an error kind.
pub(crate) fn status_name(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidRequest | ErrorKind::TooLarge => "INVALID_ARGUMENT",
        ErrorKind::Authentication => "UNAUTHENTICATED",
        ErrorKind::Permission => "PERMISSION_DENIED",
        ErrorKind::NotFound => "NOT_FOUND",
        ErrorKind::RateLimit => "RESOURCE_EXHAUSTED",
        ErrorKind::Upstream | ErrorKind::Internal => "INTERNAL",
        ErrorKind::Unavailable => "UNAVAILABLE",
        ErrorKind::Timeout => "DEADLINE_EXCEEDED",
    }
}

/// Renders an error in Google's envelope. The same object is used as the
/// in-stream error payload.
///
/// The message is redacted once more on the way out: an error another
/// protocol's decoder took from its upstream is rendered here too, and this
/// is the last place to catch a credential before it reaches a client.
pub(crate) fn encode_error(error: &ApiError) -> Value {
    let message = redact_secrets(&error.message);
    let mut inner = Map::new();
    inner.insert("code".to_string(), Value::from(error.status));
    inner.insert("message".to_string(), Value::String(message.clone()));
    inner.insert("status".to_string(), Value::from(status_name(error.kind)));
    let mut details = Vec::new();
    if let Some(code) = &error.code {
        details.push(json!({"@type": ERROR_INFO, "reason": code.to_ascii_uppercase(), "domain": "switchyard"}));
    }
    if let Some(param) = &error.param {
        details.push(json!({
            "@type": BAD_REQUEST,
            "fieldViolations": [{"field": param, "description": message}],
        }));
    }
    if let Some(secs) = error.retry_after_secs {
        details.push(json!({"@type": RETRY_INFO, "retryDelay": format!("{secs}s")}));
    }
    if !details.is_empty() {
        inner.insert("details".to_string(), Value::Array(details));
    }
    json!({"error": inner})
}

fn kind_from_status_name(status: &str) -> Option<ErrorKind> {
    Some(match status {
        "INVALID_ARGUMENT" | "FAILED_PRECONDITION" | "OUT_OF_RANGE" => ErrorKind::InvalidRequest,
        "UNAUTHENTICATED" => ErrorKind::Authentication,
        "PERMISSION_DENIED" => ErrorKind::Permission,
        "NOT_FOUND" => ErrorKind::NotFound,
        "RESOURCE_EXHAUSTED" => ErrorKind::RateLimit,
        "UNAVAILABLE" => ErrorKind::Unavailable,
        "DEADLINE_EXCEEDED" => ErrorKind::Timeout,
        "INTERNAL" | "UNKNOWN" | "DATA_LOSS" | "ABORTED" | "CANCELLED" => ErrorKind::Upstream,
        _ => return None,
    })
}

/// Turns an error object found *inside a stream* (`{"error": {...}}` as an
/// SSE payload) into the error reported to the client.
pub(crate) fn api_error_from_payload(payload: &Value) -> ApiError {
    let info = info_from_json(payload).unwrap_or_else(|| UpstreamErrorInfo {
        message: clean_message(&payload.to_string()),
        ..UpstreamErrorInfo::default()
    });
    let http = find_detail(payload)
        .and_then(|d| d.get("code"))
        .and_then(Value::as_u64)
        .filter(|code| (400..=599).contains(code))
        .map(|code| code as u16);
    let kind = info
        .error_type
        .as_deref()
        .and_then(kind_from_status_name)
        .or_else(|| http.map(ErrorKind::from_status))
        // An upstream failing mid-stream is not the client's fault.
        .unwrap_or(ErrorKind::Upstream);
    let kind = match kind {
        // 500 from the upstream is an upstream error, not a gateway bug.
        ErrorKind::Internal => ErrorKind::Upstream,
        other => other,
    };
    let mut error = ApiError::new(kind, info.message);
    error.code = info.code.or(info.error_type);
    if let Some(ms) = info.retry_after_ms {
        error.retry_after_secs = Some(ms.div_ceil(1000).max(1));
    }
    error
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
        retry_after_ms: retry_hint_in_text(trimmed),
    }
}

/// Upstream error text as it may be shown: credentials removed, then cut to
/// the longest message kept.
pub(crate) fn clean_message(text: &str) -> String {
    truncate_chars(&redact_secrets(text), MAX_MESSAGE_CHARS)
}

/// Locates the error object. Google wraps streamed errors in a one-element
/// array (`[{"error": {...}}]`); compatible servers use assorted shapes.
fn find_detail(value: &Value) -> Option<&Value> {
    match value {
        Value::Array(items) => items.iter().find_map(find_detail),
        Value::Object(map) => {
            if let Some(error) = map.get("error").filter(|e| !e.is_null()) {
                return Some(error);
            }
            if let Some(error) = map
                .get("response")
                .and_then(|r| r.get("error"))
                .filter(|e| !e.is_null())
            {
                return Some(error);
            }
            (map.contains_key("message") || map.contains_key("detail")).then_some(value)
        }
        _ => None,
    }
}

fn scalar_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) if !text.trim().is_empty() => Some(redact_secrets(text.trim())),
        Some(Value::Number(number)) => Some(number.to_string()),
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
            // Google carries the class in `status`; OpenAI-compatible and
            // Anthropic-on-Vertex bodies carry it in `type`.
            info.error_type = pick_str(detail, &["status", "type"])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let details = map.get("details").and_then(Value::as_array);
            let reason = details.and_then(|d| error_info_reason(d));
            info.code = if details.is_some_and(|d| is_daily_quota(d)) {
                Some(CODE_DAILY_QUOTA.to_string())
            } else {
                // A numeric `code` is just the HTTP status again.
                reason.or_else(|| {
                    scalar_string(map.get("code")).filter(|c| c.parse::<u64>().is_err())
                })
            };
            info.retry_after_ms = details.and_then(|d| retry_from_details(d));
        }
        _ => return None,
    }
    if info.retry_after_ms.is_none() {
        info.retry_after_ms = retry_hint_in_text(&info.message);
    }
    if info.message.trim().is_empty() {
        info.message = info
            .error_type
            .clone()
            .or_else(|| info.code.clone())
            .unwrap_or_else(|| clean_message(&value.to_string()));
    } else {
        info.message = clean_message(info.message.trim());
    }
    Some(info)
}

fn detail_type(detail: &Value) -> &str {
    detail.get("@type").and_then(Value::as_str).unwrap_or("")
}

fn error_info_reason(details: &[Value]) -> Option<String> {
    details
        .iter()
        .filter(|d| detail_type(d) == ERROR_INFO)
        .find_map(|d| scalar_string(d.get("reason")))
}

/// Whether a `QuotaFailure` names a per-day quota. Gemini's quota ids look
/// like `GenerateRequestsPerDayPerProjectPerModel-FreeTier` versus
/// `GenerateRequestsPerMinutePerProjectPerModel`.
fn is_daily_quota(details: &[Value]) -> bool {
    details
        .iter()
        .filter(|d| detail_type(d) == QUOTA_FAILURE)
        .filter_map(|d| d.get("violations").and_then(Value::as_array))
        .flatten()
        .any(|violation| {
            ["quotaId", "quotaMetric", "subject", "description"]
                .iter()
                .any(|field| {
                    violation
                        .get(*field)
                        .and_then(Value::as_str)
                        .is_some_and(|text| {
                            let text = text.to_ascii_lowercase();
                            text.contains("perday")
                                || text.contains("per_day")
                                || text.contains("per day")
                        })
                })
        })
}

fn retry_from_details(details: &[Value]) -> Option<u64> {
    let from_retry_info = details
        .iter()
        .filter(|d| detail_type(d) == RETRY_INFO)
        .find_map(|d| {
            d.get("retryDelay")
                .or_else(|| d.get("retry_delay"))
                .and_then(duration_value_ms)
        });
    from_retry_info.or_else(|| {
        details
            .iter()
            .filter(|d| detail_type(d) == ERROR_INFO)
            .find_map(|d| {
                d.get("metadata")
                    .and_then(|m| m.get("quotaResetDelay"))
                    .and_then(duration_value_ms)
            })
    })
}

/// A `google.protobuf.Duration` in JSON: normally `"12.5s"`, occasionally the
/// `{"seconds": 12, "nanos": 500000000}` object form.
fn duration_value_ms(value: &Value) -> Option<u64> {
    match value {
        Value::String(text) => parse_duration_ms(text),
        Value::Number(seconds) => seconds
            .as_f64()
            .filter(|s| *s >= 0.0)
            .map(|s| (s * 1000.0).round() as u64),
        Value::Object(map) => {
            let seconds = map
                .get("seconds")
                .and_then(crate::util::num_u64)
                .unwrap_or(0);
            let nanos = map.get("nanos").and_then(crate::util::num_u64).unwrap_or(0);
            Some(
                seconds
                    .saturating_mul(1000)
                    .saturating_add(nanos / 1_000_000),
            )
        }
        _ => None,
    }
}

/// Parses `12s`, `1.5s`, `500ms`, `1m30s`, `1h2m3.5s`, or a bare number of
/// seconds, into milliseconds.
pub(crate) fn parse_duration_ms(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut total_ms = 0f64;
    let mut rest = text;
    let mut pieces = 0;
    while !rest.is_empty() {
        let digits = rest
            .char_indices()
            .find(|(_, c)| !(c.is_ascii_digit() || *c == '.'))
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let number: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit_len = rest
            .char_indices()
            .find(|(_, c)| c.is_ascii_digit() || *c == '.')
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let factor = match rest[..unit_len].trim() {
            "h" => 3_600_000.0,
            "m" => 60_000.0,
            "s" => 1000.0,
            "ms" => 1.0,
            "us" | "µs" => 0.001,
            "ns" => 0.000_001,
            // A bare number is a count of seconds.
            "" if pieces == 0 => 1000.0,
            _ => return None,
        };
        rest = &rest[unit_len..];
        total_ms += number * factor;
        pieces += 1;
    }
    (total_ms.is_finite() && total_ms >= 0.0).then(|| total_ms.round() as u64)
}

/// Finds a retry delay stated in prose: "Please retry in 12.3s.",
/// "try again in 32 seconds", "retry after 500ms".
fn retry_hint_in_text(text: &str) -> Option<u64> {
    const MARKERS: [&str; 6] = [
        "retry in ",
        "retry after ",
        "try again in ",
        "try again after ",
        "reset after ",
        "retrydelay: ",
    ];
    let lower = text.to_ascii_lowercase();
    for marker in MARKERS {
        let Some(start) = lower.find(marker) else {
            continue;
        };
        let mut words = lower[start + marker.len()..].split_whitespace();
        let Some(first) = words.next() else {
            continue;
        };
        let token =
            first.trim_matches(|c: char| matches!(c, '"' | '\'' | '(' | ')' | ',' | ';' | ':'));
        let token = token.strip_suffix('.').unwrap_or(token);
        let has_unit = token.chars().any(|c| c.is_ascii_alphabetic());
        if has_unit {
            if let Some(ms) = parse_duration_ms(token) {
                return Some(ms);
            }
            continue;
        }
        let Ok(number) = token.parse::<f64>() else {
            continue;
        };
        let unit = words.next().unwrap_or("");
        let factor = if unit.starts_with("ms") || unit.starts_with("millisecond") {
            1.0
        } else if unit.starts_with("min") {
            60_000.0
        } else if unit.starts_with('h') {
            3_600_000.0
        } else {
            1000.0
        };
        let ms = number * factor;
        if ms.is_finite() && ms >= 0.0 {
            return Some(ms.round() as u64);
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
    head.starts_with('<') || head.contains("<html") || head.contains("<!doctype")
}

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
    (!title.is_empty()).then(|| truncate_chars(&title, 200))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("12s"), Some(12_000));
        assert_eq!(parse_duration_ms("1.5s"), Some(1_500));
        assert_eq!(parse_duration_ms("0.5s"), Some(500));
        assert_eq!(parse_duration_ms("500ms"), Some(500));
        assert_eq!(parse_duration_ms("1m30s"), Some(90_000));
        assert_eq!(parse_duration_ms("1h2m3.5s"), Some(3_723_500));
        assert_eq!(parse_duration_ms("32"), Some(32_000));
        assert_eq!(parse_duration_ms(" 7s "), Some(7_000));
        assert_eq!(parse_duration_ms("soon"), None);
        assert_eq!(parse_duration_ms("12 parsecs"), None);
        assert_eq!(parse_duration_ms(""), None);
        assert_eq!(parse_duration_ms("1.2.3s"), None);
    }

    #[test]
    fn prose_retry_hints() {
        assert_eq!(
            retry_hint_in_text("Quota exceeded. Please retry in 12.3s."),
            Some(12_300)
        );
        assert_eq!(
            retry_hint_in_text("Please try again in 32 seconds"),
            Some(32_000)
        );
        assert_eq!(retry_hint_in_text("retry after 500ms, thanks"), Some(500));
        assert_eq!(retry_hint_in_text("Retry in 2 minutes."), Some(120_000));
        assert_eq!(
            retry_hint_in_text("quota will reset after 1h2m3s"),
            Some(3_723_000)
        );
        assert_eq!(retry_hint_in_text("retry in a while"), None);
        assert_eq!(retry_hint_in_text("nothing here"), None);
    }

    #[test]
    fn html_titles() {
        assert_eq!(
            html_title("<html><head><title>502\n Bad   Gateway</title></head></html>").as_deref(),
            Some("502 Bad Gateway")
        );
        assert_eq!(html_title("<html><body>oops</body></html>"), None);
        assert!(looks_like_html("  <!DOCTYPE html><html>".trim()));
        assert!(!looks_like_html("upstream connect error"));
    }
}
