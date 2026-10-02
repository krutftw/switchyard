//! Turns a failed upstream response into an [`UpstreamError`].
//!
//! Three things are decided here, from the status **and** the body:
//!
//! * the [`FailureClass`] — what the scheduler does with the credential;
//! * the retry hint — how long the upstream asked us to wait;
//! * the vendor's own message, type and code, parsed out of the OpenAI,
//!   Anthropic and Google error envelopes (or summarised from HTML / plain
//!   text when a proxy or CDN answered instead of the API).
//!
//! # Classification
//!
//! | condition | class |
//! |---|---|
//! | 402 | `Quota` |
//! | 429 with a billing / quota-exhaustion body (`insufficient_quota`, spend limits, a per-day limit such as a Google `QuotaFailure` on a `…PerDay…` quota) | `Quota` |
//! | any other 429 | `RateLimit` |
//! | 401 | `Auth` |
//! | 403 naming a model or a region the model is not served in | `ModelNotFound` |
//! | 403 / 404 from Google about a File or CachedContent the request refers to | `Request` |
//! | any other 403 | `Auth` |
//! | 400 from Google with `API_KEY_INVALID` (Google answers bad keys with 400) | `Auth` |
//! | 400 / 422 whose body says the model does not exist or is unsupported | `ModelNotFound` |
//! | 404 | `ModelNotFound`, unless the body carries a request-fault code |
//! | 400 from Anthropic for a spend limit or an empty credit balance | `Quota` |
//! | a request-fault code or type in the body (`context_length_exceeded`, `invalid_request_error`, …) | `Request` |
//! | 400 from Google with status `FAILED_PRECONDITION` about billing, the free tier or the caller's location | `Auth` |
//! | any other 400 `FAILED_PRECONDITION` from Google (a state of the project, not of the request) | `Server` |
//! | 400, 409, 413, 422 | `Request` |
//! | 408, 499, 426 | `Transport` |
//! | 5xx (including 529), bot-challenge pages, redirects and **every other status** (405, 407, 421, 451, …) | `Server` |
//!
//! Only 400, 409, 413 and 422 are request faults by status alone. Any other
//! status an upstream — or a proxy or CDN in front of it — answers with says
//! nothing about the client's request, so the next credential is tried.
//!
//! The classification looks at one response. Whether a 404 or 405 from an
//! optional endpoint (token counting, a raw side endpoint) should count
//! against the credential at all is for the caller to decide: it knows which
//! operation it sent.
//!
//! A retry hint of zero, or one that already lies in the past, is reported
//! as "no hint" so the scheduler falls back to its own back-off instead of
//! retrying in a tight loop.

use http::HeaderMap;
use http::header::{CONNECTION, CONTENT_TYPE, HeaderName, RETRY_AFTER};
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use switchyard_core::config::ProviderKind;
use switchyard_core::util::truncate_chars;
use switchyard_core::{FailureClass, Protocol, UpstreamError, UpstreamErrorInfo};

/// Largest error body kept on an [`UpstreamError`].
pub const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Longest vendor message kept in [`UpstreamErrorInfo::message`].
const MAX_MESSAGE_CHARS: usize = 2000;

/// Excerpt length for bodies that are not a recognised error envelope.
const EXCERPT_CHARS: usize = 500;

/// Wait assumed for a tokens-per-minute limit reported without any hint.
const TPM_FALLBACK_MS: u64 = 60_000;

/// Body codes / types that mean "the request itself is at fault".
const REQUEST_CODES: &[&str] = &[
    "cyber_policy",
    "context_length_exceeded",
    "context_too_large",
    "message_too_big",
    "string_above_max_length",
    "invalid_prompt",
    "invalid_value",
    "unsupported_value",
    "invalid_request_error",
    "previous_response_not_found",
    "request_too_large",
];
const REQUEST_TYPES: &[&str] = &[
    "invalid_request",
    "invalid_request_error",
    "bad_request_error",
    "invalid_prompt",
    "request_too_large",
];

/// Body codes / types that mean "this credential cannot use this model".
const MODEL_CODES: &[&str] = &[
    "model_not_found",
    "model_not_found_error",
    "unknown_model",
    "model_does_not_exist",
    "model_not_exist",
    "model_not_supported",
    "unsupported_model",
    "deploymentnotfound",
    "deployment_not_found",
];

/// Body codes / types that mean "the account is out of money or quota".
const QUOTA_CODES: &[&str] = &[
    "insufficient_quota",
    "billing_hard_limit_reached",
    "billing_not_active",
    "billing_error",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
    "enforced_spend_limit_reached",
    "usage_limit_reached",
    "insufficient_balance",
    "insufficient_user_quota",
];

/// Message fragments with the same meaning, for envelopes without a code.
/// Deliberately narrow: ordinary rate-limit messages mention "billing" too
/// ("…add a payment method at /account/billing").
const QUOTA_PHRASES: &[&str] = &[
    "exceeded your current quota",
    "check your plan and billing details",
    "credit balance is too low",
    "insufficient balance",
    "insufficient credits",
    "insufficient_quota",
    "billing hard limit",
    "reached your specified api usage limits",
    "reached your specified workspace api usage limits",
];

/// Which vendor's error envelope a body is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavour {
    /// `{"error":{"message","type","code","param"}}` and its many imitations.
    Openai,
    /// `{"type":"error","error":{"type","message"},"request_id"}`.
    Anthropic,
    /// `{"error":{"code","message","status","details":[…]}}`.
    Google,
    /// Not a JSON error envelope (HTML, plain text, empty).
    Opaque,
}

/// Everything the classifier needs from a body.
#[derive(Clone, Debug)]
struct Parsed {
    info: UpstreamErrorInfo,
    flavour: Flavour,
    /// Normalised (lower-case, `_`-separated) codes found in the body.
    codes: Vec<String>,
    /// Normalised types / statuses found in the body.
    types: Vec<String>,
    message_lower: String,
    /// The whole body, lower-cased, for marker searches.
    text_lower: String,
    /// A Google `QuotaFailure` names a per-day quota.
    daily_quota: bool,
}

impl Parsed {
    fn has_code(&self, set: &[&str]) -> bool {
        self.codes
            .iter()
            .chain(self.types.iter())
            .any(|c| set.contains(&c.as_str()))
    }

    fn says(&self, needle: &str) -> bool {
        self.message_lower.contains(needle)
    }
}

// ---------------------------------------------------------------------------
// Durations and timestamps
// ---------------------------------------------------------------------------

/// Parses a Go-style duration string — `1s`, `6m0s`, `120ms`, `1.5s`,
/// `1h2m3s` — into milliseconds, rounding up. Returns `None` for anything
/// else, including a bare number.
pub fn parse_duration_ms(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut rest = text;
    let mut total_ms = 0f64;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if digits == 0 {
            return None;
        }
        let number: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let factor = match &rest[..unit_len] {
            "ns" => 1e-6,
            "us" | "µs" | "μs" => 1e-3,
            "ms" => 1.0,
            "s" => 1_000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            _ => return None,
        };
        total_ms += number * factor;
        rest = &rest[unit_len..];
    }
    finite_ms(total_ms)
}

fn finite_ms(ms: f64) -> Option<u64> {
    if !ms.is_finite() || ms < 0.0 {
        return None;
    }
    // Round away binary floating-point noise (1.2 s must not become
    // 1200.0000000000002 ms and then 1201) before rounding up.
    let ms = (ms * 1e3).round() / 1e3;
    Some(ms.ceil().min(u64::MAX as f64) as u64)
}

/// Milliseconds from `now` until `when`; `None` when `when` is not in the
/// future.
fn until(when: SystemTime, now: SystemTime) -> Option<u64> {
    when.duration_since(now)
        .ok()
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .filter(|ms| *ms > 0)
}

fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let parsed = chrono::DateTime::parse_from_rfc3339(text.trim()).ok()?;
    let millis = parsed.timestamp_millis();
    if millis < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_millis(millis as u64))
}

/// A `Retry-After` value: delay seconds (fractions allowed), an HTTP-date or
/// an RFC 3339 timestamp.
fn parse_retry_after(value: &str, now: SystemTime) -> Option<u64> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<f64>() {
        return finite_ms(seconds * 1000.0).filter(|ms| *ms > 0);
    }
    if let Ok(date) = httpdate::parse_http_date(value) {
        return until(date, now);
    }
    parse_rfc3339(value).and_then(|date| until(date, now))
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// A rate-limit window described by a `remaining` and a `reset` header.
struct Window {
    exhausted: bool,
    reset_ms: u64,
}

/// OpenAI-style `x-ratelimit-reset-*`: a Go duration; some compatible
/// servers send plain seconds.
fn openai_window(headers: &HeaderMap, bucket: &str) -> Option<Window> {
    let raw = header_str(headers, &format!("x-ratelimit-reset-{bucket}"))?;
    let reset_ms = parse_duration_ms(raw).or_else(|| {
        raw.parse::<f64>()
            .ok()
            // A large bare number is an epoch timestamp, not a delay.
            .filter(|seconds| *seconds < 1e8)
            .and_then(|seconds| finite_ms(seconds * 1000.0))
    })?;
    let exhausted = header_str(headers, &format!("x-ratelimit-remaining-{bucket}"))
        .and_then(|v| v.parse::<f64>().ok())
        .is_some_and(|remaining| remaining <= 0.0);
    (reset_ms > 0).then_some(Window {
        exhausted,
        reset_ms,
    })
}

/// Anthropic-style `anthropic-ratelimit-*-reset`: an RFC 3339 timestamp.
fn anthropic_window(headers: &HeaderMap, bucket: &str, now: SystemTime) -> Option<Window> {
    let raw = header_str(headers, &format!("anthropic-ratelimit-{bucket}-reset"))?;
    let reset_ms = until(parse_rfc3339(raw)?, now)?;
    let exhausted = header_str(headers, &format!("anthropic-ratelimit-{bucket}-remaining"))
        .and_then(|v| v.parse::<f64>().ok())
        .is_some_and(|remaining| remaining <= 0.0);
    Some(Window {
        exhausted,
        reset_ms,
    })
}

/// The wait implied by rate-limit window headers: when a window is known to
/// be exhausted, the time until every exhausted window resets; otherwise the
/// soonest reset (some capacity is back by then).
fn window_hint(headers: &HeaderMap, now: SystemTime) -> Option<u64> {
    let mut windows: Vec<Window> = Vec::new();
    for bucket in ["requests", "tokens"] {
        windows.extend(openai_window(headers, bucket));
    }
    for bucket in ["requests", "tokens", "input-tokens", "output-tokens"] {
        windows.extend(anthropic_window(headers, bucket, now));
    }
    let exhausted = windows
        .iter()
        .filter(|w| w.exhausted)
        .map(|w| w.reset_ms)
        .max();
    exhausted.or_else(|| windows.iter().map(|w| w.reset_ms).min())
}

/// The wait an upstream asked for through response headers, in
/// milliseconds: `retry-after-ms`, then `Retry-After` (seconds, HTTP-date or
/// RFC 3339). Zero and past values yield `None`.
pub fn retry_after_from_headers(headers: &HeaderMap, now: SystemTime) -> Option<u64> {
    if let Some(ms) = header_str(headers, "retry-after-ms")
        .and_then(|v| v.parse::<f64>().ok())
        .and_then(finite_ms)
        .filter(|ms| *ms > 0)
    {
        return Some(ms);
    }
    headers
        .get_all(RETRY_AFTER)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| parse_retry_after(v, now))
}

// ---------------------------------------------------------------------------
// Body parsing
// ---------------------------------------------------------------------------

/// Lower-cases a code or type and unifies separators, so `Model-Not-Found`,
/// `MODEL_NOT_FOUND` and `model_not_found` compare equal.
fn normalize_code(raw: &str) -> String {
    raw.trim()
        .chars()
        .map(|c| match c {
            '-' | ' ' | '.' => '_',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

fn non_empty(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// A JSON scalar rendered as text (`"x"` → `x`, `42` → `42`).
fn scalar_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn is_html(text: &str, content_type: Option<&str>) -> bool {
    if content_type.is_some_and(|ct| ct.to_ascii_lowercase().contains("html")) {
        return true;
    }
    let head: String = text
        .trim_start()
        .chars()
        .take(15)
        .collect::<String>()
        .to_ascii_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html")
}

fn html_title(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    // `to_ascii_lowercase` keeps byte offsets, so they are valid in `text`.
    let title = text
        .get(start..end)?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!title.is_empty()).then_some(title)
}

fn status_phrase(status: u16) -> String {
    match http::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
    {
        Some(reason) => format!("HTTP {status} {reason}"),
        None => format!("HTTP {status}"),
    }
}

/// The wait stated inside a message: "try again in 20s", "Please retry in
/// 1.5s", "retry after 30 seconds", "try again in 6m0s".
fn retry_hint_in_message(message_lower: &str) -> Option<u64> {
    const MARKERS: &[&str] = &[
        "try again in ",
        "retry in ",
        "retrying in ",
        "retry after ",
        "try again after ",
        "reset after ",
    ];
    for marker in MARKERS {
        let mut search = message_lower;
        while let Some(pos) = search.find(marker) {
            let tail = &search[pos + marker.len()..];
            if let Some(ms) = leading_duration(tail) {
                return Some(ms);
            }
            search = tail;
        }
    }
    None
}

/// Parses the duration a text starts with: a Go duration (`6m0s`), or a
/// number followed by a unit word (`30 seconds`, `1.5 s`).
fn leading_duration(text: &str) -> Option<u64> {
    let text = text.trim_start();
    let text = text
        .strip_prefix("about ")
        .or_else(|| text.strip_prefix("approximately "))
        .unwrap_or(text)
        .trim_start();
    let token_len = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == 'µ' || c == 'μ'))
        .unwrap_or(text.len());
    let token = text[..token_len].trim_end_matches('.');
    if let Some(ms) = parse_duration_ms(token) {
        return Some(ms).filter(|ms| *ms > 0);
    }
    // "<number> <unit word>" or "<number><unit word>".
    let digits = token
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(token.len());
    let number: f64 = token[..digits].trim_end_matches('.').parse().ok()?;
    let unit = if digits < token.len() {
        &token[digits..]
    } else {
        let after = text[token_len..].trim_start();
        let end = after
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(after.len());
        &after[..end]
    };
    let factor = match unit {
        "ms" | "millisecond" | "milliseconds" => 1.0,
        "s" | "sec" | "secs" | "second" | "seconds" => 1_000.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
        _ => return None,
    };
    finite_ms(number * factor).filter(|ms| *ms > 0)
}

/// A `google.protobuf.Duration` in its JSON form (`"32.5s"`), or the
/// expanded `{seconds, nanos}` form.
fn proto_duration_ms(value: &Value) -> Option<u64> {
    match value {
        Value::String(s) => parse_duration_ms(s),
        Value::Object(map) => {
            let seconds = map.get("seconds").and_then(|v| match v {
                Value::String(s) => s.parse::<f64>().ok(),
                other => other.as_f64(),
            });
            let nanos = map.get("nanos").and_then(Value::as_f64);
            if seconds.is_none() && nanos.is_none() {
                return None;
            }
            finite_ms(seconds.unwrap_or(0.0) * 1000.0 + nanos.unwrap_or(0.0) / 1e6)
        }
        _ => None,
    }
}

fn names_daily_quota(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    ["perday", "per_day", "per day", "per-day", "daily"]
        .iter()
        .any(|marker| t.contains(marker))
}

/// Reads the `details` array of a Google error.
fn google_details(
    details: &[Value],
    codes: &mut Vec<String>,
    retry_ms: &mut Option<u64>,
    daily_quota: &mut bool,
) -> Option<String> {
    let mut reason = None;
    let mut quota_reset = None;
    for detail in details {
        let kind = detail.get("@type").and_then(Value::as_str).unwrap_or("");
        if kind.ends_with("google.rpc.RetryInfo") {
            if let Some(ms) = detail.get("retryDelay").and_then(proto_duration_ms) {
                retry_ms.get_or_insert(ms);
            }
        } else if kind.ends_with("google.rpc.ErrorInfo") {
            if let Some(r) = non_empty(detail.get("reason")) {
                codes.push(normalize_code(r));
                reason.get_or_insert_with(|| r.to_string());
            }
            if let Some(ms) = detail
                .get("metadata")
                .and_then(|m| m.get("quotaResetDelay"))
                .and_then(proto_duration_ms)
            {
                quota_reset.get_or_insert(ms);
            }
        } else if kind.ends_with("google.rpc.QuotaFailure") {
            let violations = detail.get("violations").and_then(Value::as_array);
            for violation in violations.into_iter().flatten() {
                for key in ["quotaId", "quotaMetric", "subject", "description"] {
                    if non_empty(violation.get(key)).is_some_and(names_daily_quota) {
                        *daily_quota = true;
                    }
                }
            }
        }
    }
    if retry_ms.is_none() {
        *retry_ms = quota_reset;
    }
    reason
}

/// OpenAI `usage_limit_reached` bodies say when the limit resets.
fn usage_limit_reset_ms(root: &Value, error: Option<&Value>, now: SystemTime) -> Option<u64> {
    for holder in [error, Some(root)].into_iter().flatten() {
        let at = holder
            .get("resets_at")
            .and_then(Value::as_f64)
            .filter(|at| *at > 0.0)
            .and_then(|at| until(UNIX_EPOCH + Duration::from_secs_f64(at.min(1e11)), now));
        if at.is_some() {
            return at;
        }
        let within = holder
            .get("resets_in_seconds")
            .and_then(Value::as_f64)
            .and_then(|seconds| finite_ms(seconds * 1000.0))
            .filter(|ms| *ms > 0);
        if within.is_some() {
            return within;
        }
    }
    None
}

fn parse_body(text: &str, content_type: Option<&str>, status: u16, now: SystemTime) -> Parsed {
    let text_lower = text.to_ascii_lowercase();
    let mut parsed = Parsed {
        info: UpstreamErrorInfo::default(),
        flavour: Flavour::Opaque,
        codes: Vec::new(),
        types: Vec::new(),
        message_lower: String::new(),
        text_lower,
        daily_quota: false,
    };

    let json: Option<Value> = serde_json::from_str(text.trim()).ok();
    // Gemini streaming endpoints wrap their error in a one-element array.
    let root = match &json {
        Some(Value::Array(items)) => items.iter().find(|v| v.is_object()),
        Some(v @ Value::Object(_)) => Some(v),
        _ => None,
    };

    let Some(root) = root else {
        parsed.info.message = if text.trim().is_empty() {
            format!(
                "upstream returned {} with an empty body",
                status_phrase(status)
            )
        } else if is_html(text, content_type) {
            match html_title(text) {
                Some(title) => format!(
                    "upstream returned an HTML page ({}): {}",
                    status_phrase(status),
                    truncate_chars(&title, 200)
                ),
                None => format!("upstream returned an HTML page ({})", status_phrase(status)),
            }
        } else {
            truncate_chars(text.trim(), EXCERPT_CHARS)
        };
        parsed.message_lower = parsed.info.message.to_ascii_lowercase();
        parsed.daily_quota = names_daily_quota(&parsed.message_lower);
        parsed.info.retry_after_ms = retry_hint_in_message(&parsed.text_lower);
        return parsed;
    };

    // The error object sits in one of a few places; every one that exists
    // contributes codes and types, the first one supplies the message.
    let holders: Vec<&Value> = [
        root.get("error"),
        root.get("response").and_then(|r| r.get("error")),
        root.get("body").and_then(|b| b.get("error")),
    ]
    .into_iter()
    .flatten()
    .filter(|v| v.is_object())
    .collect();
    let error = holders.first().copied();

    let mut retry_ms: Option<u64> = None;
    let mut google_reason: Option<String> = None;

    for holder in &holders {
        if let Some(code) = non_empty(holder.get("code")) {
            parsed.codes.push(normalize_code(code));
        }
        if let Some(kind) = non_empty(holder.get("type")) {
            parsed.types.push(normalize_code(kind));
        }
        if let Some(status_name) = non_empty(holder.get("status")) {
            parsed.types.push(normalize_code(status_name));
        }
        if let Some(code) = non_empty(holder.get("details").and_then(|d| d.get("error_code"))) {
            parsed.codes.push(normalize_code(code));
        }
        if let Some(details) = holder.get("details").and_then(Value::as_array) {
            let reason = google_details(
                details,
                &mut parsed.codes,
                &mut retry_ms,
                &mut parsed.daily_quota,
            );
            if google_reason.is_none() {
                google_reason = reason;
            }
        }
    }
    if let Some(code) = non_empty(root.get("code")) {
        parsed.codes.push(normalize_code(code));
    }
    if let Some(kind) = non_empty(root.get("type")).filter(|t| !t.eq_ignore_ascii_case("error")) {
        parsed.types.push(normalize_code(kind));
    }

    // Which envelope is this?
    let google_status = error
        .and_then(|e| non_empty(e.get("status")))
        .is_some_and(|s| s.chars().all(|c| c.is_ascii_uppercase() || c == '_'));
    let google_details_present = error
        .and_then(|e| e.get("details"))
        .and_then(Value::as_array)
        .is_some_and(|d| d.iter().any(|item| item.get("@type").is_some()));
    parsed.flavour = if google_status || google_details_present {
        Flavour::Google
    } else if root.get("type").and_then(Value::as_str) == Some("error") {
        Flavour::Anthropic
    } else {
        Flavour::Openai
    };

    // Message.
    let message = error
        .and_then(|e| non_empty(e.get("message")).or_else(|| non_empty(e.get("msg"))))
        .map(str::to_string)
        .or_else(|| {
            // `{"error":"text"}`, optionally with an OAuth-style description.
            let short = non_empty(root.get("error"))?;
            Some(match non_empty(root.get("error_description")) {
                Some(description) => format!("{short}: {description}"),
                None => short.to_string(),
            })
        })
        .or_else(|| non_empty(root.get("message")).map(str::to_string))
        .or_else(|| non_empty(root.get("msg")).map(str::to_string))
        .or_else(|| match root.get("detail") {
            // FastAPI: a string, or a list of validation problems.
            Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
            Some(other @ (Value::Array(_) | Value::Object(_))) => Some(other.to_string()),
            _ => None,
        })
        .or_else(|| error.and_then(|e| scalar_text(e.get("code"))))
        .or_else(|| error.and_then(|e| scalar_text(e.get("type"))))
        .unwrap_or_else(|| truncate_chars(text.trim(), EXCERPT_CHARS));
    parsed.info.message = truncate_chars(&message, MAX_MESSAGE_CHARS);
    parsed.message_lower = parsed.info.message.to_ascii_lowercase();

    // Type and code as the vendor spelled them.
    parsed.info.error_type = error
        .and_then(|e| match parsed.flavour {
            Flavour::Google => non_empty(e.get("status")),
            _ => non_empty(e.get("type")),
        })
        .or_else(|| non_empty(root.get("type")).filter(|t| !t.eq_ignore_ascii_case("error")))
        .map(str::to_string);
    parsed.info.code = match parsed.flavour {
        // Google's numeric `code` only repeats the HTTP status; the useful
        // machine-readable value is `ErrorInfo.reason`.
        Flavour::Google => google_reason,
        Flavour::Anthropic => error
            .and_then(|e| non_empty(e.get("details").and_then(|d| d.get("error_code"))))
            .map(str::to_string),
        _ => error
            .and_then(|e| scalar_text(e.get("code")))
            .or_else(|| scalar_text(root.get("code")))
            // A numeric code equal to the status says nothing new.
            .filter(|code| *code != status.to_string()),
    };
    if parsed.info.code == parsed.info.error_type {
        parsed.info.code = None;
    }

    if !parsed.daily_quota {
        parsed.daily_quota = names_daily_quota(&parsed.message_lower);
    }
    // Retry hints inside the body.
    parsed.info.retry_after_ms = retry_ms
        .filter(|ms| *ms > 0)
        .or_else(|| usage_limit_reset_ms(root, error, now))
        .or_else(|| retry_hint_in_message(&parsed.message_lower));
    parsed
}

/// Extracts the vendor's message, type, code and in-body retry hint from an
/// error response body. Understands the OpenAI, Anthropic and Google
/// envelopes (and the common variations compatible servers produce); HTML
/// pages are reduced to their title and other text to a short excerpt.
pub fn parse_error_body(body: &str, content_type: Option<&str>, status: u16) -> UpstreamErrorInfo {
    parse_body(body, content_type, status, SystemTime::now()).info
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

fn is_challenge_page(parsed: &Parsed, headers: &HeaderMap) -> bool {
    if header_str(headers, "cf-mitigated").is_some() {
        return true;
    }
    let t = &parsed.text_lower;
    t.contains("challenge-platform")
        || t.contains("cf-mitigated")
        || t.contains("cloudflare challenge")
        || (t.contains("just a moment") && t.contains("cloudflare"))
}

/// The body says the model is unknown to, or unavailable for, this
/// credential.
fn names_missing_model(parsed: &Parsed) -> bool {
    if !parsed.says("model") {
        return false;
    }
    const DIRECT: &[&str] = &[
        "model_not_found",
        "model not found",
        "no such model",
        "unknown model",
        "model_not_supported",
        "model is not supported",
        "model not supported",
        "unsupported model",
        "model unavailable",
        "model is unavailable",
        "not available for your plan",
        "not available for your account",
        "does not have access to model",
        "do not have access to it",
        "is not found for api version",
        "not supported for generatecontent",
    ];
    if DIRECT.iter().any(|p| parsed.says(p)) {
        return true;
    }
    // "The model `x` does not exist": only when the sentence is about the
    // model itself, not about something missing "in the request".
    if parsed.says("in request") || parsed.says("in body") {
        return false;
    }
    let model_at = parsed.message_lower.find("model").unwrap_or(usize::MAX);
    ["does not exist", "not exist", "not found", "was not found"]
        .iter()
        .filter_map(|p| parsed.message_lower.find(p))
        .any(|at| at > model_at)
}

/// A 403 saying the model is not served in the configured region.
fn names_unserved_region(parsed: &Parsed) -> bool {
    parsed.says("model")
        && (parsed.says("region") || parsed.says("location"))
        && [
            "not available",
            "not supported",
            "not servable",
            "unsupported",
            "not enabled",
        ]
        .iter()
        .any(|p| parsed.says(p))
}

fn is_quota_body(parsed: &Parsed) -> bool {
    if parsed.has_code(QUOTA_CODES) {
        return true;
    }
    match parsed.flavour {
        // Gemini words per-minute throttling exactly like quota exhaustion
        // ("You exceeded your current quota…"); only a per-day quota id
        // tells them apart, which the 429 rule checks for every envelope.
        Flavour::Google => false,
        _ => QUOTA_PHRASES.iter().any(|p| parsed.says(p)),
    }
}

/// Google answers an invalid, expired or leaked API key with a 400.
fn is_bad_key_body(parsed: &Parsed) -> bool {
    parsed.codes.iter().any(|c| {
        matches!(
            c.as_str(),
            "api_key_invalid" | "api_key_expired" | "api_key_service_blocked" | "invalid_api_key"
        )
    }) || parsed.says("api key not valid")
        || parsed.says("api key expired")
        || parsed.says("api key was reported as leaked")
}

/// Anthropic rejects `speed: "fast"` without credits with a 429 that every
/// key of the account would repeat; treating it as a quota failure would
/// cool the whole pool because of one request's option.
fn is_fast_mode_entitlement(parsed: &Parsed) -> bool {
    parsed.says("fast request rejected")
        || (parsed.says("fast")
            && (parsed.says("usage credits") || parsed.says("credits are required")))
}

/// Google refuses a request that refers to an uploaded File or a
/// CachedContent owned by another project with a 403 (or 404). The
/// credential is fine — the request names something it cannot see — so the
/// credential must not be rested for it.
fn names_foreign_resource(parsed: &Parsed) -> bool {
    parsed.flavour == Flavour::Google
        && (parsed.says("access the file")
            || parsed.message_lower.starts_with("file ")
            || parsed.says("files/")
            || parsed.says("cachedcontent")
            || parsed.says("cached content"))
}

/// A Google `FAILED_PRECONDITION`: the project behind the key is not in a
/// state to serve the call (billing not enabled, free tier not offered in
/// the caller's country, …).
fn is_failed_precondition(parsed: &Parsed) -> bool {
    parsed.flavour == Flavour::Google && parsed.types.iter().any(|t| t == "failed_precondition")
}

/// The wordings of a `FAILED_PRECONDITION` that will not go away by
/// waiting: they describe the key's project or where it is called from.
fn is_account_precondition(parsed: &Parsed) -> bool {
    [
        "billing",
        "free tier",
        "in your country",
        "location is not supported",
    ]
    .iter()
    .any(|p| parsed.says(p))
}

fn decide(status: u16, parsed: &Parsed, headers: &HeaderMap) -> FailureClass {
    match status {
        402 => return FailureClass::Quota,
        429 => {
            return if parsed.flavour == Flavour::Anthropic && is_fast_mode_entitlement(parsed) {
                FailureClass::Request
            } else if is_quota_body(parsed) || parsed.daily_quota {
                // A per-day allowance that is used up will not come back
                // within any ordinary back-off.
                FailureClass::Quota
            } else {
                FailureClass::RateLimit
            };
        }
        401 => return FailureClass::Auth,
        _ => {}
    }
    if status < 500 && is_challenge_page(parsed, headers) {
        // A bot check in front of the API: neither the request nor the
        // credential is at fault.
        return FailureClass::Server;
    }
    if status == 403 {
        return if parsed.has_code(MODEL_CODES)
            || names_missing_model(parsed)
            || names_unserved_region(parsed)
        {
            FailureClass::ModelNotFound
        } else if is_quota_body(parsed) {
            FailureClass::Quota
        } else if names_foreign_resource(parsed) {
            FailureClass::Request
        } else {
            FailureClass::Auth
        };
    }
    if status == 400 {
        if is_bad_key_body(parsed) || parsed.says("location is not supported") {
            return FailureClass::Auth;
        }
        if is_quota_body(parsed) {
            return FailureClass::Quota;
        }
    }
    if status < 500 && parsed.has_code(MODEL_CODES) {
        return FailureClass::ModelNotFound;
    }
    if matches!(status, 400 | 404 | 422) && names_missing_model(parsed) {
        return FailureClass::ModelNotFound;
    }
    let request_fault = parsed
        .codes
        .iter()
        .any(|c| REQUEST_CODES.contains(&c.as_str()))
        || parsed
            .types
            .iter()
            .any(|t| REQUEST_TYPES.contains(&t.as_str()))
        || (parsed.says("item with id")
            && parsed.says("not found")
            && parsed.says("items are not persisted"));
    if request_fault {
        return FailureClass::Request;
    }
    if status == 400 && is_failed_precondition(parsed) {
        // Not the request's fault: a credential of another project can
        // serve the very same call.
        return if is_account_precondition(parsed) {
            FailureClass::Auth
        } else {
            FailureClass::Server
        };
    }
    match status {
        404 if names_foreign_resource(parsed) => FailureClass::Request,
        404 => FailureClass::ModelNotFound,
        // 408: the upstream gave up waiting; 499: something between us and
        // it closed the request. Either way no answer was produced.
        408 | 499 => FailureClass::Transport,
        // A WebSocket endpoint that refuses to upgrade: another route may
        // still serve the request over HTTP.
        426 => FailureClass::Transport,
        // The only statuses that blame the request by themselves.
        400 | 409 | 413 | 422 => FailureClass::Request,
        // 5xx, an unfollowed redirect (the base URL is wrong), and every
        // other 4xx — a forward proxy's 407, a CDN's 451, a 405 or 421 from
        // a mis-routed endpoint: nothing says the request is at fault, so
        // another credential or provider gets its chance.
        _ => FailureClass::Server,
    }
}

/// Cuts `bytes` to at most `max` bytes of valid UTF-8.
fn lossy_prefix(bytes: &[u8], max: usize) -> String {
    let slice = &bytes[..bytes.len().min(max)];
    match std::str::from_utf8(slice) {
        Ok(s) => s.to_string(),
        // A multi-byte character cut at the limit: keep the valid part.
        Err(e) if slice.len() < bytes.len() && e.error_len().is_none() => {
            String::from_utf8_lossy(&slice[..e.valid_up_to()]).into_owned()
        }
        Err(_) => String::from_utf8_lossy(slice).into_owned(),
    }
}

/// Builds the [`UpstreamError`] for a non-2xx upstream response.
///
/// `kind` and `protocol` say which upstream answered; the envelope itself is
/// recognised from the body, because compatible servers and proxies answer
/// in whatever dialect they like. `body` may be arbitrarily large: all of it
/// is read for the classification, at most [`MAX_ERROR_BODY_BYTES`] of it are
/// kept on the error.
///
/// The body is taken as given. [`crate::UpstreamClient::send`] removes the
/// credentials it presented from the body before calling this; a caller
/// classifying a body from elsewhere should pass it through
/// [`crate::Target::redact`] first.
pub fn classify(
    kind: ProviderKind,
    protocol: Protocol,
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
) -> UpstreamError {
    classify_at(kind, protocol, status, headers, body, SystemTime::now())
}

/// [`classify`] with an explicit clock, so absolute retry timestamps can be
/// tested.
pub fn classify_at(
    kind: ProviderKind,
    protocol: Protocol,
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
    now: SystemTime,
) -> UpstreamError {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    // The envelope is parsed from everything that arrived — a JSON document
    // cut at the size limit would not parse at all — and only the copy kept
    // on the error is truncated.
    let parsed = {
        let full = String::from_utf8_lossy(body);
        parse_body(&full, content_type.as_deref(), status, now)
    };
    let text = lossy_prefix(body, MAX_ERROR_BODY_BYTES);
    let class = decide(status, &parsed, headers);

    let mut retry_after_ms =
        retry_after_from_headers(headers, now).or(parsed.info.retry_after_ms.filter(|ms| *ms > 0));
    if status == 429 && retry_after_ms.is_none() {
        retry_after_ms = window_hint(headers, now).or_else(|| {
            // Some providers omit every hint for per-minute token limits;
            // replaying a large request immediately would only fail again.
            let tpm = parsed
                .codes
                .iter()
                .any(|c| c.contains("tpmratelimitexceeded"))
                || (parsed.says("tokens per minute")
                    && parsed.says("limit")
                    && parsed.says("exceeded"));
            tpm.then_some(TPM_FALLBACK_MS)
        });
    }

    // `kind` and `protocol` are part of the signature so that callers
    // state which upstream answered; the envelope is recognised from the
    // body alone (see above). Nothing is logged here: the caller knows which
    // credentials to keep out of the log.
    let _ = (kind, protocol);

    UpstreamError {
        status,
        class,
        info: parsed.info,
        retry_after_ms,
        body: (!text.is_empty()).then_some(text),
        content_type,
    }
}

// ---------------------------------------------------------------------------
// Response headers
// ---------------------------------------------------------------------------

/// Removes the upstream response headers that must never be relayed to a
/// client: hop-by-hop headers (including any named by `Connection`),
/// `Set-Cookie`, the body framing headers (the gateway decompresses and
/// re-frames bodies itself) and the CORS headers the gateway owns.
pub fn filter_response_headers(headers: &HeaderMap) -> HeaderMap {
    const DROPPED: &[&str] = &[
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "set-cookie",
        "set-cookie2",
        "content-length",
        "content-encoding",
        "access-control-allow-credentials",
        "access-control-allow-headers",
        "access-control-allow-methods",
        "access-control-allow-origin",
        "access-control-expose-headers",
        "access-control-max-age",
    ];
    let named_by_connection: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect();
    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if DROPPED.contains(&name.as_str()) || named_by_connection.contains(name) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    const JSON: (&str, &str) = ("content-type", "application/json");

    /// A fixed "now": 2026-10-02T12:00:00Z.
    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_790_942_400)
    }

    fn run(
        kind: ProviderKind,
        protocol: Protocol,
        status: u16,
        hdrs: &[(&str, &str)],
        body: &str,
    ) -> UpstreamError {
        classify_at(
            kind,
            protocol,
            status,
            &headers(hdrs),
            body.as_bytes(),
            now(),
        )
    }

    fn openai(status: u16, hdrs: &[(&str, &str)], body: &Value) -> UpstreamError {
        let mut all = vec![JSON];
        all.extend_from_slice(hdrs);
        run(
            ProviderKind::Openai,
            Protocol::OpenaiResponses,
            status,
            &all,
            &body.to_string(),
        )
    }

    fn anthropic(status: u16, hdrs: &[(&str, &str)], body: &Value) -> UpstreamError {
        let mut all = vec![JSON];
        all.extend_from_slice(hdrs);
        run(
            ProviderKind::Anthropic,
            Protocol::Anthropic,
            status,
            &all,
            &body.to_string(),
        )
    }

    fn gemini(status: u16, body: &Value) -> UpstreamError {
        run(
            ProviderKind::Gemini,
            Protocol::Gemini,
            status,
            &[JSON],
            &body.to_string(),
        )
    }

    // ------------------------------------------------------- durations

    #[test]
    fn go_durations() {
        let cases = [
            ("1s", Some(1_000)),
            ("6m0s", Some(360_000)),
            ("120ms", Some(120)),
            ("1.5s", Some(1_500)),
            ("32.5s", Some(32_500)),
            ("0.5s", Some(500)),
            ("1h2m3s", Some(3_723_000)),
            ("2h", Some(7_200_000)),
            ("1m30.5s", Some(90_500)),
            ("500us", Some(1)),
            ("0s", Some(0)),
            (" 20s ", Some(20_000)),
            ("", None),
            ("20", None),
            ("s", None),
            ("1x", None),
            ("abc", None),
            ("1s2", None),
            ("1..5s", None),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_duration_ms(input), expected, "{input:?}");
        }
    }

    #[test]
    fn message_hints() {
        let cases = [
            ("Rate limit reached. Please try again in 20s.", Some(20_000)),
            ("please try again in 1.2s. visit the docs", Some(1_200)),
            ("Please try again in 6m0s.", Some(360_000)),
            ("Please try again in 804ms.", Some(804)),
            ("Quota exceeded. Please retry in 32.5s.", Some(32_500)),
            ("Please retry in 1.5s", Some(1_500)),
            ("retry after 30 seconds", Some(30_000)),
            ("Try again in 2 minutes", Some(120_000)),
            ("try again in about 1 hour", Some(3_600_000)),
            ("quota will reset after 34s", Some(34_000)),
            ("try again in a while", None),
            ("try again later", None),
            ("no hint at all", None),
            ("try again in 0s", None),
        ];
        for (message, expected) in cases {
            assert_eq!(
                retry_hint_in_message(&message.to_ascii_lowercase()),
                expected,
                "{message:?}"
            );
        }
    }

    // ---------------------------------------------------- retry headers

    #[test]
    fn retry_after_header_forms() {
        let at = |pairs: &[(&str, &str)]| retry_after_from_headers(&headers(pairs), now());
        assert_eq!(at(&[("retry-after", "56")]), Some(56_000));
        assert_eq!(at(&[("Retry-After", "1.5")]), Some(1_500));
        assert_eq!(at(&[("retry-after", " 7 ")]), Some(7_000));
        // 90 s after the fixed clock.
        assert_eq!(
            at(&[("retry-after", "Fri, 02 Oct 2026 12:01:30 GMT")]),
            Some(90_000)
        );
        assert_eq!(at(&[("retry-after", "2026-10-02T12:00:45Z")]), Some(45_000));
        // Past, zero and garbage values are "no hint".
        assert_eq!(
            at(&[("retry-after", "Fri, 02 Oct 2026 11:00:00 GMT")]),
            None
        );
        assert_eq!(at(&[("retry-after", "0")]), None);
        assert_eq!(at(&[("retry-after", "soon")]), None);
        assert_eq!(at(&[("retry-after", "-5")]), None);
        assert_eq!(at(&[]), None);
        // The millisecond header is more precise and wins.
        assert_eq!(
            at(&[("retry-after", "2"), ("retry-after-ms", "1234.5")]),
            Some(1_235)
        );
    }

    #[test]
    fn openai_reset_headers_are_used_for_429_without_retry_after() {
        let body = json!({"error": {"message": "Rate limit reached for requests", "type": "requests", "code": "rate_limit_exceeded"}});
        // Request window exhausted: wait for it, not for the token window.
        let e = openai(
            429,
            &[
                ("x-ratelimit-remaining-requests", "0"),
                ("x-ratelimit-reset-requests", "6m0s"),
                ("x-ratelimit-remaining-tokens", "149000"),
                ("x-ratelimit-reset-tokens", "120ms"),
            ],
            &body,
        );
        assert_eq!(e.class, FailureClass::RateLimit);
        assert_eq!(e.retry_after_ms, Some(360_000));

        // Nothing marked exhausted: the soonest reset.
        let e = openai(
            429,
            &[
                ("x-ratelimit-reset-requests", "1s"),
                ("x-ratelimit-reset-tokens", "6m0s"),
            ],
            &body,
        );
        assert_eq!(e.retry_after_ms, Some(1_000));

        // Retry-After beats the windows.
        let e = openai(
            429,
            &[("retry-after", "3"), ("x-ratelimit-reset-requests", "6m0s")],
            &body,
        );
        assert_eq!(e.retry_after_ms, Some(3_000));

        // The windows are informational on other statuses.
        let e = openai(500, &[("x-ratelimit-reset-requests", "6m0s")], &body);
        assert_eq!(e.retry_after_ms, None);
    }

    #[test]
    fn anthropic_reset_headers_are_rfc3339() {
        let body = json!({"type": "error", "error": {"type": "rate_limit_error", "message": "This request would exceed your organization's rate limit of 50 requests per minute."}, "request_id": "req_011"});
        let e = anthropic(
            429,
            &[
                ("anthropic-ratelimit-requests-remaining", "0"),
                ("anthropic-ratelimit-requests-reset", "2026-10-02T12:00:30Z"),
                ("anthropic-ratelimit-tokens-remaining", "12000"),
                ("anthropic-ratelimit-tokens-reset", "2026-10-02T12:00:05Z"),
            ],
            &body,
        );
        assert_eq!(e.class, FailureClass::RateLimit);
        assert_eq!(e.retry_after_ms, Some(30_000));
        assert_eq!(e.info.error_type.as_deref(), Some("rate_limit_error"));

        let e = anthropic(429, &[("retry-after", "12")], &body);
        assert_eq!(e.retry_after_ms, Some(12_000));

        // A reset in the past is ignored.
        let e = anthropic(
            429,
            &[("anthropic-ratelimit-requests-reset", "2026-10-02T11:59:00Z")],
            &body,
        );
        assert_eq!(e.retry_after_ms, None);
    }

    // ------------------------------------------------------------ OpenAI

    #[test]
    fn openai_rate_limit_with_hint_in_message() {
        let e = openai(
            429,
            &[],
            &json!({"error": {
                "message": "Rate limit reached for gpt-5 in organization org-abc on tokens per min (TPM): Limit 30000, Used 29500, Requested 1200. Please try again in 1.4s. Visit https://platform.openai.com/account/rate-limits to learn more. You can increase your rate limit by adding a payment method to your account at https://platform.openai.com/account/billing.",
                "type": "tokens",
                "param": null,
                "code": "rate_limit_exceeded"
            }}),
        );
        // "billing" in the boilerplate must not turn this into a quota error.
        assert_eq!(e.class, FailureClass::RateLimit);
        assert_eq!(e.status, 429);
        assert_eq!(e.retry_after_ms, Some(1_400));
        assert_eq!(e.info.retry_after_ms, Some(1_400));
        assert_eq!(e.info.error_type.as_deref(), Some("tokens"));
        assert_eq!(e.info.code.as_deref(), Some("rate_limit_exceeded"));
        assert!(e.info.message.starts_with("Rate limit reached for gpt-5"));
        assert_eq!(e.content_type.as_deref(), Some("application/json"));
        assert!(e.body.as_deref().unwrap().contains("rate_limit_exceeded"));
    }

    #[test]
    fn openai_insufficient_quota_is_quota() {
        let e = openai(
            429,
            &[],
            &json!({"error": {
                "message": "You exceeded your current quota, please check your plan and billing details. For more information on this error, read the docs: https://platform.openai.com/docs/guides/error-codes/api-errors.",
                "type": "insufficient_quota",
                "param": null,
                "code": "insufficient_quota"
            }}),
        );
        assert_eq!(e.class, FailureClass::Quota);
        assert_eq!(e.retry_after_ms, None);
        assert_eq!(e.info.error_type.as_deref(), Some("insufficient_quota"));
        // Code equal to the type is not repeated.
        assert_eq!(e.info.code, None);
    }

    #[test]
    fn openai_billing_codes_are_quota() {
        for code in [
            "billing_hard_limit_reached",
            "credit_balance_exhausted",
            "organization_spend_limit_exceeded",
            "project_spend_limit_exceeded",
            "organization_usage_limit_exceeded",
        ] {
            let e = openai(
                429,
                &[],
                &json!({"error": {"message": "Limit reached.", "type": "insufficient_quota", "code": code}}),
            );
            assert_eq!(e.class, FailureClass::Quota, "{code}");
        }
        // Phrase only, no code.
        let e = openai(
            429,
            &[],
            &json!({"error": {"message": "You exceeded your current quota.", "type": "error"}}),
        );
        assert_eq!(e.class, FailureClass::Quota);
    }

    #[test]
    fn exhausted_daily_allowances_are_quota() {
        // OpenAI requests-per-day limit: a 429 like any other, but the
        // window is a day. The stated wait is still reported.
        let e = openai(
            429,
            &[],
            &json!({"error": {
                "message": "Rate limit reached for gpt-5 in organization org-abc on requests per day (RPD): Limit 200, Used 200, Requested 1. Please try again in 7m12s.",
                "type": "requests",
                "param": null,
                "code": "rate_limit_exceeded"
            }}),
        );
        assert_eq!(e.class, FailureClass::Quota);
        assert_eq!(e.retry_after_ms, Some(432_000));

        // An aggregator's free tier.
        let e = run(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            429,
            &[JSON],
            r#"{"error":{"message":"Rate limit exceeded: free-models-per-day. Add credits to unlock more requests.","code":429}}"#,
        );
        assert_eq!(e.class, FailureClass::Quota);

        // The word alone does not turn other statuses into quota failures.
        let e = openai(
            400,
            &[],
            &json!({"error": {"message": "The daily digest parameter is invalid.", "type": "invalid_request_error"}}),
        );
        assert_eq!(e.class, FailureClass::Request);
    }

    #[test]
    fn usage_limit_reached_carries_its_reset() {
        let e = openai(
            429,
            &[],
            &json!({"error": {"type": "usage_limit_reached", "message": "The usage limit has been reached", "resets_in_seconds": 3600}}),
        );
        assert_eq!(e.class, FailureClass::Quota);
        assert_eq!(e.retry_after_ms, Some(3_600_000));

        let resets_at = 1_790_942_400 + 120;
        let e = openai(
            429,
            &[],
            &json!({"error": {"type": "usage_limit_reached", "message": "limit", "resets_at": resets_at}}),
        );
        assert_eq!(e.retry_after_ms, Some(120_000));
    }

    #[test]
    fn payment_required_is_quota() {
        let e = run(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            402,
            &[JSON],
            r#"{"error":{"message":"Insufficient Balance","type":"unknown_error","param":null,"code":"invalid_request_error"}}"#,
        );
        assert_eq!(e.class, FailureClass::Quota);
        assert_eq!(e.info.message, "Insufficient Balance");
    }

    #[test]
    fn openai_auth_failures() {
        // OpenAI labels a bad key `invalid_request_error`; a 401 is still a
        // credential fault.
        let e = openai(
            401,
            &[],
            &json!({"error": {
                "message": "Incorrect API key provided: sk-proj-********************abcd. You can find your API key at https://platform.openai.com/account/api-keys.",
                "type": "invalid_request_error",
                "param": null,
                "code": "invalid_api_key"
            }}),
        );
        assert_eq!(e.class, FailureClass::Auth);
        assert_eq!(e.info.code.as_deref(), Some("invalid_api_key"));

        let e = openai(
            403,
            &[],
            &json!({"error": {"message": "Country, region, or territory not supported", "type": "request_forbidden", "param": null, "code": "unsupported_country_region_territory"}}),
        );
        assert_eq!(e.class, FailureClass::Auth);
    }

    #[test]
    fn openai_model_access() {
        let e = openai(
            404,
            &[],
            &json!({"error": {"message": "The model `gpt-9` does not exist or you do not have access to it.", "type": "invalid_request_error", "param": null, "code": "model_not_found"}}),
        );
        assert_eq!(e.class, FailureClass::ModelNotFound);

        let e = openai(
            403,
            &[],
            &json!({"error": {"message": "Project `proj_abc` does not have access to model `gpt-6-astra`", "type": "invalid_request_error", "param": null, "code": "model_not_found"}}),
        );
        assert_eq!(e.class, FailureClass::ModelNotFound);

        // Compatible servers report it with a 400.
        let e = run(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            400,
            &[JSON],
            r#"{"error":{"message":"Model llama-99 does not exist","type":"invalid_request_error"}}"#,
        );
        assert_eq!(e.class, FailureClass::ModelNotFound);
    }

    #[test]
    fn openai_request_faults() {
        let e = openai(
            400,
            &[],
            &json!({"error": {
                "message": "This model's maximum context length is 128000 tokens. However, your messages resulted in 130532 tokens. Please reduce the length of the messages.",
                "type": "invalid_request_error",
                "param": "messages",
                "code": "context_length_exceeded"
            }}),
        );
        assert_eq!(e.class, FailureClass::Request);
        assert_eq!(e.info.code.as_deref(), Some("context_length_exceeded"));

        // A parameter the model rejects mentions "model" and "not supported"
        // without being a missing model.
        let e = openai(
            400,
            &[],
            &json!({"error": {"message": "Unsupported parameter: 'temperature' is not supported with this model.", "type": "invalid_request_error", "param": "temperature", "code": "unsupported_parameter"}}),
        );
        assert_eq!(e.class, FailureClass::Request);

        let e = openai(
            400,
            &[],
            &json!({"error": {"message": "Previous response with id 'resp_abc' not found.", "type": "invalid_request_error", "param": "previous_response_id", "code": "previous_response_not_found"}}),
        );
        assert_eq!(e.class, FailureClass::Request);

        // The same code on a 404 is still the request's fault.
        let e = openai(
            404,
            &[],
            &json!({"error": {"message": "Item with id 'rs_1' not found. Items are not persisted when `store` is set to false.", "type": "invalid_request_error", "param": "input", "code": null}}),
        );
        assert_eq!(e.class, FailureClass::Request);

        for status in [409, 413, 422] {
            let e = openai(status, &[], &json!({"error": {"message": "nope"}}));
            assert_eq!(e.class, FailureClass::Request, "{status}");
        }
    }

    #[test]
    fn openai_server_side_failures() {
        let e = openai(
            503,
            &[("retry-after", "5")],
            &json!({"error": {"message": "The server is overloaded.", "type": "service_unavailable_error", "code": "server_is_overloaded"}}),
        );
        assert_eq!(e.class, FailureClass::Server);
        assert_eq!(e.retry_after_ms, Some(5_000));

        let e = openai(
            500,
            &[],
            &json!({"error": {"message": "The server had an error while processing your request.", "type": "server_error", "code": null}}),
        );
        assert_eq!(e.class, FailureClass::Server);
        assert_eq!(e.info.code, None);

        // The ramp-rate throttle is a 429 now and an ordinary rate limit.
        let e = openai(
            429,
            &[],
            &json!({"error": {"message": "Slow down.", "type": "rate_limit_error", "code": "slow_down"}}),
        );
        assert_eq!(e.class, FailureClass::RateLimit);
    }

    #[test]
    fn tpm_limit_without_any_hint_waits_a_minute() {
        let e = run(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            429,
            &[JSON],
            r#"{"error":{"message":"Tokens per minute limit exceeded for this deployment","code":"TPMRateLimitExceeded"}}"#,
        );
        assert_eq!(e.class, FailureClass::RateLimit);
        assert_eq!(e.retry_after_ms, Some(60_000));
    }

    // --------------------------------------------------------- Anthropic

    #[test]
    fn anthropic_status_table() {
        let cases = [
            (
                400,
                "invalid_request_error",
                "max_tokens: Field required",
                FailureClass::Request,
            ),
            (
                401,
                "authentication_error",
                "invalid x-api-key",
                FailureClass::Auth,
            ),
            (402, "billing_error", "Billing issue", FailureClass::Quota),
            (
                403,
                "permission_error",
                "Your API key does not have permission to use the specified resource.",
                FailureClass::Auth,
            ),
            (
                404,
                "not_found_error",
                "model: claude-nope-9",
                FailureClass::ModelNotFound,
            ),
            (409, "conflict_error", "conflict", FailureClass::Request),
            (
                413,
                "request_too_large",
                "Request exceeds the maximum allowed number of bytes.",
                FailureClass::Request,
            ),
            (
                429,
                "rate_limit_error",
                "Number of request tokens has exceeded your per-minute rate limit",
                FailureClass::RateLimit,
            ),
            (
                500,
                "api_error",
                "Internal server error",
                FailureClass::Server,
            ),
            (
                504,
                "timeout_error",
                "Request timed out",
                FailureClass::Server,
            ),
            (529, "overloaded_error", "Overloaded", FailureClass::Server),
        ];
        for (status, kind, message, class) in cases {
            let e = anthropic(
                status,
                &[],
                &json!({"type": "error", "error": {"type": kind, "message": message}, "request_id": "req_011CSHoEeqs5C35K2UUqR7Fy"}),
            );
            assert_eq!(e.class, class, "{status} {kind}");
            assert_eq!(e.status, status);
            assert_eq!(e.info.error_type.as_deref(), Some(kind));
            assert_eq!(e.info.message, message);
            assert_eq!(e.info.code, None);
        }
    }

    #[test]
    fn anthropic_spend_limits_are_quota() {
        // Monthly tier cap: a 429 without retry-after.
        let e = anthropic(
            429,
            &[],
            &json!({"type": "error", "error": {"type": "rate_limit_error", "message": "You have reached your monthly spend limit.", "details": {"error_code": "enforced_spend_limit_reached"}}}),
        );
        assert_eq!(e.class, FailureClass::Quota);
        assert_eq!(e.info.code.as_deref(), Some("enforced_spend_limit_reached"));
        assert_eq!(e.retry_after_ms, None);

        // User-set limits and empty balances arrive as 400.
        for message in [
            "You have reached your specified API usage limits. You will regain access on 2026-11-01 at 00:00 UTC.",
            "You have reached your specified workspace API usage limits. You will regain access on 2026-11-01 at 00:00 UTC.",
            "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.",
        ] {
            let e = anthropic(
                400,
                &[],
                &json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}}),
            );
            assert_eq!(e.class, FailureClass::Quota, "{message}");
        }
    }

    #[test]
    fn anthropic_ordinary_400s_stay_request_faults() {
        for message in [
            "\"thinking.type.enabled\" is not supported for this model. Use \"thinking.type.adaptive\" and \"output_config.effort\" to control thinking behavior.",
            "adaptive thinking is not supported on this model",
            "This model does not support assistant message prefill. The conversation must end with a user message.",
            "prompt is too long: 250000 tokens > 200000 maximum",
            "Unexpected value(s) `made-up-2099-01-01` for the `anthropic-beta` header.",
        ] {
            let e = anthropic(
                400,
                &[],
                &json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}}),
            );
            assert_eq!(e.class, FailureClass::Request, "{message}");
        }
    }

    #[test]
    fn anthropic_fast_mode_entitlement_does_not_cool_the_pool() {
        let e = anthropic(
            429,
            &[],
            &json!({"type": "error", "error": {"type": "rate_limit_error", "message": "Fast request rejected: usage credits are required for fast mode."}}),
        );
        assert_eq!(e.class, FailureClass::Request);
    }

    // ------------------------------------------------------------ Google

    fn gemini_429(quota_id: &str, metric: &str, retry: Option<&str>) -> Value {
        let mut details = vec![json!({
            "@type": "type.googleapis.com/google.rpc.QuotaFailure",
            "violations": [{
                "quotaMetric": metric,
                "quotaId": quota_id,
                "quotaDimensions": {"location": "global", "model": "gemini-2.5-pro"},
                "quotaValue": "50"
            }]
        })];
        details.push(json!({
            "@type": "type.googleapis.com/google.rpc.Help",
            "links": [{"description": "Learn more about Gemini API quotas", "url": "https://ai.google.dev/gemini-api/docs/rate-limits"}]
        }));
        if let Some(delay) = retry {
            details.push(
                json!({"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": delay}),
            );
        }
        json!({"error": {
            "code": 429,
            "message": "You exceeded your current quota, please check your plan and billing details. For more information on this error, head to: https://ai.google.dev/gemini-api/docs/rate-limits.",
            "status": "RESOURCE_EXHAUSTED",
            "details": details
        }})
    }

    #[test]
    fn gemini_per_minute_quota_is_a_rate_limit_with_retry_info() {
        let e = gemini(
            429,
            &gemini_429(
                "GenerateRequestsPerMinutePerProjectPerModel-FreeTier",
                "generativelanguage.googleapis.com/generate_content_free_tier_requests",
                Some("32.5s"),
            ),
        );
        // The message says "exceeded your current quota", yet this is plain
        // per-minute throttling.
        assert_eq!(e.class, FailureClass::RateLimit);
        assert_eq!(e.retry_after_ms, Some(32_500));
        assert_eq!(e.info.error_type.as_deref(), Some("RESOURCE_EXHAUSTED"));
        assert_eq!(e.info.code, None);
    }

    #[test]
    fn gemini_per_day_quota_is_quota() {
        let e = gemini(
            429,
            &gemini_429(
                "GenerateRequestsPerDayPerProjectPerModel-FreeTier",
                "generativelanguage.googleapis.com/generate_content_free_tier_requests",
                Some("41s"),
            ),
        );
        assert_eq!(e.class, FailureClass::Quota);
        // The hint is still reported; the scheduler decides what to do.
        assert_eq!(e.retry_after_ms, Some(41_000));

        let e = gemini(
            429,
            &gemini_429(
                "x",
                "generativelanguage.googleapis.com/requests_per_day",
                None,
            ),
        );
        assert_eq!(e.class, FailureClass::Quota);
    }

    #[test]
    fn google_retry_hint_fallbacks() {
        // ErrorInfo metadata.
        let e = gemini(
            429,
            &json!({"error": {"code": 429, "message": "Resource has been exhausted (e.g. check quota).", "status": "RESOURCE_EXHAUSTED", "details": [
                {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "RATE_LIMIT_EXCEEDED", "domain": "googleapis.com", "metadata": {"quotaResetDelay": "1m2s"}}
            ]}}),
        );
        assert_eq!(e.class, FailureClass::RateLimit);
        assert_eq!(e.retry_after_ms, Some(62_000));
        assert_eq!(e.info.code.as_deref(), Some("RATE_LIMIT_EXCEEDED"));

        // Message only.
        let e = gemini(
            429,
            &json!({"error": {"code": 429, "message": "Quota exceeded for metric: generate_content_requests. Please retry in 7.25s.", "status": "RESOURCE_EXHAUSTED"}}),
        );
        assert_eq!(e.retry_after_ms, Some(7_250));

        // Expanded Duration object.
        let e = gemini(
            429,
            &json!({"error": {"code": 429, "message": "slow down", "status": "RESOURCE_EXHAUSTED", "details": [
                {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": {"seconds": "3", "nanos": 500000000}}
            ]}}),
        );
        assert_eq!(e.retry_after_ms, Some(3_500));

        // Vertex pay-as-you-go wording: no hint at all.
        let e = run(
            ProviderKind::Vertex,
            Protocol::Gemini,
            429,
            &[JSON],
            r#"{"error":{"code":429,"message":"Resource exhausted, please try again later.","status":"RESOURCE_EXHAUSTED"}}"#,
        );
        assert_eq!(e.class, FailureClass::RateLimit);
        assert_eq!(e.retry_after_ms, None);
    }

    #[test]
    fn gemini_invalid_key_is_a_400_auth_failure() {
        let e = gemini(
            400,
            &json!({"error": {"code": 400, "message": "API key not valid. Please pass a valid API key.", "status": "INVALID_ARGUMENT", "details": [
                {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "API_KEY_INVALID", "domain": "googleapis.com", "metadata": {"service": "generativelanguage.googleapis.com"}},
                {"@type": "type.googleapis.com/google.rpc.LocalizedMessage", "locale": "en-US", "message": "API key not valid. Please pass a valid API key."}
            ]}}),
        );
        assert_eq!(e.class, FailureClass::Auth);
        assert_eq!(e.info.error_type.as_deref(), Some("INVALID_ARGUMENT"));
        assert_eq!(e.info.code.as_deref(), Some("API_KEY_INVALID"));

        let e = gemini(
            400,
            &json!({"error": {"code": 400, "message": "User location is not supported for the API use.", "status": "FAILED_PRECONDITION"}}),
        );
        assert_eq!(e.class, FailureClass::Auth);

        let e = gemini(
            403,
            &json!({"error": {"code": 403, "message": "Your API key was reported as leaked. Please use another API key.", "status": "PERMISSION_DENIED"}}),
        );
        assert_eq!(e.class, FailureClass::Auth);
    }

    #[test]
    fn google_status_table() {
        let cases = [
            (
                400,
                "INVALID_ARGUMENT",
                "Invalid JSON payload received. Unknown name \"foo\": Cannot find field.",
                FailureClass::Request,
            ),
            (
                402,
                "RESOURCE_EXHAUSTED",
                "Your prepayment credits are depleted.",
                FailureClass::Quota,
            ),
            (
                403,
                "PERMISSION_DENIED",
                "Permission denied on resource project my-proj.",
                FailureClass::Auth,
            ),
            (
                404,
                "NOT_FOUND",
                "models/gemini-9-pro is not found for API version v1beta, or is not supported for generateContent. Call ListModels to see the list of available models and their supported methods.",
                FailureClass::ModelNotFound,
            ),
            (
                499,
                "CANCELLED",
                "The operation was cancelled.",
                FailureClass::Transport,
            ),
            (
                500,
                "INTERNAL",
                "An internal error has occurred.",
                FailureClass::Server,
            ),
            (
                503,
                "UNAVAILABLE",
                "The model is overloaded. Please try again later.",
                FailureClass::Server,
            ),
            (
                504,
                "DEADLINE_EXCEEDED",
                "Deadline expired before operation could complete.",
                FailureClass::Server,
            ),
        ];
        for (status, name, message, class) in cases {
            let e = gemini(
                status,
                &json!({"error": {"code": status, "message": message, "status": name}}),
            );
            assert_eq!(e.class, class, "{status} {name}");
            assert_eq!(e.info.error_type.as_deref(), Some(name));
            assert_eq!(e.info.message, message);
            assert_eq!(e.info.code, None);
        }
    }

    #[test]
    fn vertex_permission_denied_for_a_model_or_region() {
        let e = run(
            ProviderKind::Vertex,
            Protocol::Anthropic,
            403,
            &[JSON],
            r#"{"error":{"code":403,"message":"Publisher Model `projects/p/locations/us-east5/publishers/anthropic/models/claude-opus-5` is not available in region us-east5 for this project.","status":"PERMISSION_DENIED"}}"#,
        );
        assert_eq!(e.class, FailureClass::ModelNotFound);

        let e = run(
            ProviderKind::Vertex,
            Protocol::Gemini,
            404,
            &[JSON],
            r#"{"error":{"code":404,"message":"Publisher Model `projects/p/locations/global/publishers/google/models/gemini-9` was not found or your project does not have access to it.","status":"NOT_FOUND"}}"#,
        );
        assert_eq!(e.class, FailureClass::ModelNotFound);

        let e = run(
            ProviderKind::Vertex,
            Protocol::Gemini,
            401,
            &[JSON],
            r#"{"error":{"code":401,"message":"Request had invalid authentication credentials. Expected OAuth 2 access token, login cookie or other valid authentication credential.","status":"UNAUTHENTICATED"}}"#,
        );
        assert_eq!(e.class, FailureClass::Auth);
    }

    #[test]
    fn gemini_array_wrapped_errors_are_understood() {
        let e = run(
            ProviderKind::Gemini,
            Protocol::Gemini,
            429,
            &[JSON],
            r#"[{"error":{"code":429,"message":"Resource has been exhausted (e.g. check quota).","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"17s"}]}}]"#,
        );
        assert_eq!(e.class, FailureClass::RateLimit);
        assert_eq!(e.retry_after_ms, Some(17_000));
        assert_eq!(
            e.info.message,
            "Resource has been exhausted (e.g. check quota)."
        );
    }

    // --------------------------------------------- non-envelope bodies

    #[test]
    fn html_bodies_are_summarised() {
        let html = "<!DOCTYPE html>\n<html><head><title>502   Bad\n Gateway</title></head><body><center><h1>502 Bad Gateway</h1></center><hr><center>nginx</center></body></html>";
        let e = run(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            500,
            &[("content-type", "text/html; charset=utf-8")],
            html,
        );
        assert_eq!(e.class, FailureClass::Server);
        assert_eq!(
            e.info.message,
            "upstream returned an HTML page (HTTP 500 Internal Server Error): 502 Bad Gateway"
        );
        assert_eq!(e.body.as_deref(), Some(html));
        assert_eq!(e.content_type.as_deref(), Some("text/html; charset=utf-8"));
        assert_eq!(e.info.error_type, None);

        // Sniffed without a content type, and without a title.
        let e = run(
            ProviderKind::Openai,
            Protocol::OpenaiChat,
            503,
            &[],
            "<html><body>down</body></html>",
        );
        assert_eq!(
            e.info.message,
            "upstream returned an HTML page (HTTP 503 Service Unavailable)"
        );
    }

    #[test]
    fn challenge_pages_are_not_credential_faults() {
        let page = "<!DOCTYPE html><html><head><title>Just a moment...</title></head><body><script src=\"/cdn-cgi/challenge-platform/h/b/orchestrate/chl_page/v1\"></script>Cloudflare</body></html>";
        let e = run(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            403,
            &[("content-type", "text/html")],
            page,
        );
        assert_eq!(e.class, FailureClass::Server);
        let e = run(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            403,
            &[("cf-mitigated", "challenge")],
            "",
        );
        assert_eq!(e.class, FailureClass::Server);
    }

    #[test]
    fn plain_text_and_empty_bodies() {
        let e = run(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            502,
            &[("content-type", "text/plain")],
            "  upstream connect error or disconnect/reset before headers  ",
        );
        assert_eq!(e.class, FailureClass::Server);
        assert_eq!(
            e.info.message,
            "upstream connect error or disconnect/reset before headers"
        );

        let e = run(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            404,
            &[],
            "",
        );
        assert_eq!(e.class, FailureClass::ModelNotFound);
        assert_eq!(
            e.info.message,
            "upstream returned HTTP 404 Not Found with an empty body"
        );
        assert_eq!(e.body, None);
        assert_eq!(e.content_type, None);

        let e = run(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            429,
            &[],
            "Too many requests, try again in 3 seconds",
        );
        assert_eq!(e.class, FailureClass::RateLimit);
        assert_eq!(e.retry_after_ms, Some(3_000));

        let long = "x".repeat(2000);
        let e = run(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            500,
            &[],
            &long,
        );
        assert_eq!(e.info.message.chars().count(), EXCERPT_CHARS + 1);
    }

    #[test]
    fn unusual_json_shapes() {
        let info = |body: &str| parse_error_body(body, Some("application/json"), 400);
        assert_eq!(
            info(r#"{"error":"model overloaded"}"#).message,
            "model overloaded"
        );
        assert_eq!(
            info(r#"{"error":"invalid_grant","error_description":"Token expired"}"#).message,
            "invalid_grant: Token expired"
        );
        assert_eq!(
            info(r#"{"message":"bad things","code":"E42"}"#).message,
            "bad things"
        );
        assert_eq!(
            info(r#"{"message":"bad things","code":"E42"}"#)
                .code
                .as_deref(),
            Some("E42")
        );
        assert_eq!(
            info(r#"{"detail":"Not authenticated"}"#).message,
            "Not authenticated"
        );
        assert!(
            info(r#"{"detail":[{"loc":["body","model"],"msg":"field required"}]}"#)
                .message
                .contains("field required")
        );
        assert_eq!(
            info(r#"{"error":{"code":"weird_code"}}"#).message,
            "weird_code"
        );
        assert_eq!(info(r#"{"error":{"message":"m","code":400}}"#).code, None);
        assert_eq!(
            info(r#"{"error":{"message":"m","code":1234}}"#)
                .code
                .as_deref(),
            Some("1234")
        );
        assert_eq!(
            info(r#"{"unrelated":true}"#).message,
            r#"{"unrelated":true}"#
        );
        // Responses failures nest the error under `response`.
        let nested = info(
            r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"boom"}}}"#,
        );
        assert_eq!(nested.message, "boom");
        assert_eq!(nested.code.as_deref(), Some("server_error"));
        assert_eq!(nested.error_type.as_deref(), Some("response.failed"));
    }

    #[test]
    fn statuses_without_a_body_rule() {
        let class = |status: u16| {
            run(
                ProviderKind::Openai,
                Protocol::OpenaiChat,
                status,
                &[],
                "{}",
            )
            .class
        };
        assert_eq!(class(408), FailureClass::Transport);
        assert_eq!(class(426), FailureClass::Transport);
        assert_eq!(class(301), FailureClass::Server);
        // Only these four blame the request by status alone …
        for status in [400, 409, 413, 422] {
            assert_eq!(class(status), FailureClass::Request, "HTTP {status}");
        }
        // … every other 4xx fails over: nothing says the request is at fault.
        for status in [405, 406, 407, 410, 411, 415, 418, 421, 423, 428, 431, 451] {
            assert_eq!(class(status), FailureClass::Server, "HTTP {status}");
        }
        assert_eq!(class(404), FailureClass::ModelNotFound);
        assert_eq!(class(499), FailureClass::Transport);
        assert_eq!(class(502), FailureClass::Server);
        assert_eq!(class(520), FailureClass::Server);
        assert_eq!(class(529), FailureClass::Server);
        assert_eq!(class(401), FailureClass::Auth);
        assert_eq!(class(402), FailureClass::Quota);
        assert_eq!(class(403), FailureClass::Auth);
        assert_eq!(class(429), FailureClass::RateLimit);
    }

    #[test]
    fn a_request_fault_code_still_wins_on_an_unlisted_4xx() {
        let e = openai(
            405,
            &[],
            &json!({"error": {"message": "nope", "type": "invalid_request_error"}}),
        );
        assert_eq!(e.class, FailureClass::Request);
    }

    fn google(status: u16, grpc_status: &str, message: &str) -> UpstreamError {
        run(
            ProviderKind::Gemini,
            Protocol::Gemini,
            status,
            &[JSON],
            &json!({"error": {"code": status, "message": message, "status": grpc_status}})
                .to_string(),
        )
    }

    #[test]
    fn google_failed_precondition_never_blames_the_request() {
        // The project behind the key cannot serve the call: rest the key.
        for message in [
            "Gemini API free tier is not available in your country. Please enable billing on your project in Google AI Studio.",
            "User location is not supported for the API use.",
            "Billing is not enabled for this project.",
        ] {
            let e = google(400, "FAILED_PRECONDITION", message);
            assert_eq!(e.class, FailureClass::Auth, "{message}");
            assert_eq!(e.info.error_type.as_deref(), Some("FAILED_PRECONDITION"));
        }
        // A precondition of unknown kind (service agents still being
        // provisioned, …): fail over, but only a short per-model rest.
        let e = google(
            400,
            "FAILED_PRECONDITION",
            "Service agents are being provisioned. Please try again in a few minutes.",
        );
        assert_eq!(e.class, FailureClass::Server);
        // The same status word in another envelope or with another HTTP
        // status is not this rule's business.
        assert_eq!(
            google(400, "INVALID_ARGUMENT", "Invalid JSON payload received.").class,
            FailureClass::Request
        );
        let e = openai(
            400,
            &[],
            &json!({"error": {"message": "billing", "type": "failed_precondition"}}),
        );
        assert_eq!(e.class, FailureClass::Request);
    }

    #[test]
    fn google_refusals_about_a_referenced_resource_are_request_faults() {
        for (status, grpc_status, message) in [
            (
                403,
                "PERMISSION_DENIED",
                "You do not have permission to access the File 8w1fdkj3tmbb or it may not exist.",
            ),
            (
                403,
                "PERMISSION_DENIED",
                "CachedContent not found (or permission denied)",
            ),
            (404, "NOT_FOUND", "File files/abc123 not found."),
            (
                404,
                "NOT_FOUND",
                "Cached content cachedContents/xyz was not found.",
            ),
        ] {
            let e = google(status, grpc_status, message);
            assert_eq!(e.class, FailureClass::Request, "{message}");
        }
        // Credential and model problems keep their classes.
        assert_eq!(
            google(
                403,
                "PERMISSION_DENIED",
                "Your API key was reported as leaked. Please use another API key."
            )
            .class,
            FailureClass::Auth
        );
        assert_eq!(
            google(
                403,
                "PERMISSION_DENIED",
                "Generative Language API has not been used in project 123 before or it is disabled."
            )
            .class,
            FailureClass::Auth
        );
        assert_eq!(
            google(
                404,
                "NOT_FOUND",
                "models/gemini-0 is not found for API version v1beta, or is not supported for generateContent."
            )
            .class,
            FailureClass::ModelNotFound
        );
        assert_eq!(
            google(404, "NOT_FOUND", "Requested entity was not found.").class,
            FailureClass::ModelNotFound
        );
        // Other vendors' 403s are not touched by the Google wording.
        let e = anthropic(
            403,
            &[],
            &json!({"type": "error", "error": {"type": "permission_error", "message": "Your API key does not have permission to access the file files/abc."}}),
        );
        assert_eq!(e.class, FailureClass::Auth);
    }

    #[test]
    fn large_envelopes_are_parsed_whole_and_kept_truncated() {
        let padding = "é".repeat(40 * 1024); // 80 KiB of two-byte characters
        let body = json!({"error": {
            "debug": padding,
            "message": "Rate limit reached. Please try again in 20s.",
            "type": "rate_limit_error",
            "code": "rate_limit_exceeded"
        }})
        .to_string();
        let e = openai_raw(429, &body);
        assert_eq!(e.class, FailureClass::RateLimit);
        assert_eq!(e.info.code.as_deref(), Some("rate_limit_exceeded"));
        assert_eq!(e.retry_after_ms, Some(20_000));
        let kept = e.body.unwrap();
        assert!(kept.len() <= MAX_ERROR_BODY_BYTES);
        assert!(kept.len() > MAX_ERROR_BODY_BYTES - 4);
        assert!(kept.starts_with(r#"{"error":{"debug":"é"#));
    }

    fn openai_raw(status: u16, body: &str) -> UpstreamError {
        run(
            ProviderKind::Openai,
            Protocol::OpenaiChat,
            status,
            &[JSON],
            body,
        )
    }

    #[test]
    fn request_fault_codes_apply_to_server_statuses_too() {
        let e = run(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            500,
            &[JSON],
            r#"{"error":{"message":"messages must not be empty","type":"invalid_request_error"}}"#,
        );
        assert_eq!(e.class, FailureClass::Request);
    }

    #[test]
    fn bodies_are_truncated_on_a_character_boundary() {
        let mut body = "é".repeat(MAX_ERROR_BODY_BYTES);
        body.insert(0, 'x');
        let e = run(ProviderKind::Openai, Protocol::OpenaiChat, 500, &[], &body);
        let kept = e.body.unwrap();
        assert!(kept.len() <= MAX_ERROR_BODY_BYTES);
        assert!(kept.len() >= MAX_ERROR_BODY_BYTES - 2);
        assert!(kept.ends_with('é'));
        assert!(!kept.contains('\u{fffd}'));

        // Binary garbage does not panic.
        let e = classify(
            ProviderKind::Openai,
            Protocol::OpenaiChat,
            500,
            &HeaderMap::new(),
            &[0xff, 0xfe, 0x00],
        );
        assert_eq!(e.class, FailureClass::Server);
    }

    #[test]
    fn the_resulting_api_error_is_sensible() {
        let e = anthropic(
            429,
            &[("retry-after", "7")],
            &json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}}),
        );
        let api = e.to_api_error();
        assert_eq!(api.status, 429);
        assert_eq!(api.retry_after_secs, Some(7));
        assert_eq!(api.code.as_deref(), Some("rate_limit_error"));
    }

    // ----------------------------------------------------- header filter

    #[test]
    fn response_header_filter() {
        let upstream = headers(&[
            ("content-type", "application/json"),
            ("x-request-id", "req_123"),
            ("request-id", "req_abc"),
            ("openai-processing-ms", "412"),
            ("x-ratelimit-remaining-requests", "99"),
            ("anthropic-ratelimit-tokens-remaining", "1000"),
            ("retry-after", "3"),
            ("connection", "keep-alive, X-Hop"),
            ("x-hop", "1"),
            ("keep-alive", "timeout=5"),
            ("transfer-encoding", "chunked"),
            ("content-length", "12"),
            ("content-encoding", "gzip"),
            ("set-cookie", "a=b"),
            ("set-cookie", "c=d"),
            ("access-control-allow-origin", "*"),
            ("access-control-expose-headers", "x-request-id"),
            ("upgrade", "h2c"),
            ("trailer", "x-t"),
            ("te", "trailers"),
            ("proxy-authenticate", "Basic"),
        ]);
        let out = filter_response_headers(&upstream);
        let mut names: Vec<&str> = out.keys().map(HeaderName::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "anthropic-ratelimit-tokens-remaining",
                "content-type",
                "openai-processing-ms",
                "request-id",
                "retry-after",
                "x-ratelimit-remaining-requests",
                "x-request-id",
            ]
        );
        assert!(filter_response_headers(&HeaderMap::new()).is_empty());
    }
}
