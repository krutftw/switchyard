//! `encode_error` and `decode_error`: Google's error envelope.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::time::Duration;
use switchyard_codec_gemini::{CODE_DAILY_QUOTA, GeminiCodec};
use switchyard_core::{ApiError, Codec, ErrorKind, UpstreamErrorInfo};

fn decode(status: u16, body: &Value) -> UpstreamErrorInfo {
    GeminiCodec.decode_error(status, body.to_string().as_bytes())
}

// ---------------------------------------------------------------------------
// encode_error
// ---------------------------------------------------------------------------

#[test]
fn encode_error_for_every_kind() {
    for (kind, code, status) in [
        (ErrorKind::InvalidRequest, 400, "INVALID_ARGUMENT"),
        (ErrorKind::Authentication, 401, "UNAUTHENTICATED"),
        (ErrorKind::Permission, 403, "PERMISSION_DENIED"),
        (ErrorKind::NotFound, 404, "NOT_FOUND"),
        (ErrorKind::TooLarge, 413, "INVALID_ARGUMENT"),
        (ErrorKind::RateLimit, 429, "RESOURCE_EXHAUSTED"),
        (ErrorKind::Internal, 500, "INTERNAL"),
        (ErrorKind::Upstream, 502, "INTERNAL"),
        (ErrorKind::Unavailable, 503, "UNAVAILABLE"),
        (ErrorKind::Timeout, 504, "DEADLINE_EXCEEDED"),
    ] {
        let error = ApiError::new(kind, format!("{kind:?} happened"));
        assert_eq!(
            GeminiCodec.encode_error(&error),
            json!({"error": {"code": code, "message": format!("{kind:?} happened"), "status": status}}),
            "{kind:?}"
        );
    }
}

#[test]
fn encode_error_uses_the_overridden_http_status() {
    let error = ApiError::rate_limit("all credentials are cooling down").with_status(503);
    assert_eq!(
        GeminiCodec.encode_error(&error),
        json!({"error": {"code": 503, "message": "all credentials are cooling down", "status": "RESOURCE_EXHAUSTED"}})
    );
}

#[test]
fn encode_error_details() {
    let error = ApiError::unknown_model("gemini-9");
    assert_eq!(
        GeminiCodec.encode_error(&error),
        json!({"error": {
            "code": 404,
            "message": "unknown model `gemini-9`: no configured provider serves it",
            "status": "NOT_FOUND",
            "details": [
                {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "MODEL_NOT_FOUND", "domain": "switchyard"},
                {"@type": "type.googleapis.com/google.rpc.BadRequest", "fieldViolations": [
                    {"field": "model", "description": "unknown model `gemini-9`: no configured provider serves it"}
                ]}
            ]
        }})
    );
    let limited = ApiError::rate_limit("slow down").with_retry_after(Duration::from_millis(12_300));
    assert_eq!(
        GeminiCodec.encode_error(&limited),
        json!({"error": {
            "code": 429,
            "message": "slow down",
            "status": "RESOURCE_EXHAUSTED",
            "details": [{"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "12s"}]
        }})
    );
}

#[test]
fn encoded_errors_decode_back() {
    let error = ApiError::rate_limit("slow down").with_retry_after(Duration::from_secs(9));
    let info = decode(429, &GeminiCodec.encode_error(&error));
    assert_eq!(info.message, "slow down");
    assert_eq!(info.error_type.as_deref(), Some("RESOURCE_EXHAUSTED"));
    assert_eq!(info.retry_after_ms, Some(9_000));
}

// ---------------------------------------------------------------------------
// decode_error: real vendor bodies
// ---------------------------------------------------------------------------

#[test]
fn decode_rate_limit_with_retry_info() {
    let body = json!({"error": {
        "code": 429,
        "message": "You exceeded your current quota, please check your plan and billing details. For more information on this error, head to: https://ai.google.dev/gemini-api/docs/rate-limits.\n* Quota exceeded for metric: generativelanguage.googleapis.com/generate_content_free_tier_requests, limit: 15\nPlease retry in 32.519742925s.",
        "status": "RESOURCE_EXHAUSTED",
        "details": [
            {"@type": "type.googleapis.com/google.rpc.QuotaFailure", "violations": [{
                "quotaMetric": "generativelanguage.googleapis.com/generate_content_free_tier_requests",
                "quotaId": "GenerateRequestsPerMinutePerProjectPerModel-FreeTier",
                "quotaDimensions": {"location": "global", "model": "gemini-2.5-flash"},
                "quotaValue": "15"
            }]},
            {"@type": "type.googleapis.com/google.rpc.Help", "links": [{"description": "Learn more about Gemini API quotas", "url": "https://ai.google.dev/gemini-api/docs/rate-limits"}]},
            {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "32s"}
        ]
    }});
    let info = decode(429, &body);
    assert!(info.message.starts_with("You exceeded your current quota"));
    assert_eq!(info.error_type.as_deref(), Some("RESOURCE_EXHAUSTED"));
    // Per-minute throttling: not a quota exhaustion.
    assert_eq!(info.code, None);
    // RetryInfo wins over the sentence in the message.
    assert_eq!(info.retry_after_ms, Some(32_000));
}

#[test]
fn decode_daily_quota_exhaustion() {
    let body = json!({"error": {
        "code": 429,
        "message": "You exceeded your current quota. Please retry in 41m12s.",
        "status": "RESOURCE_EXHAUSTED",
        "details": [
            {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "RATE_LIMIT_EXCEEDED", "domain": "googleapis.com"},
            {"@type": "type.googleapis.com/google.rpc.QuotaFailure", "violations": [{
                "quotaMetric": "generativelanguage.googleapis.com/generate_content_free_tier_requests",
                "quotaId": "GenerateRequestsPerDayPerProjectPerModel-FreeTier",
                "quotaValue": "50"
            }]}
        ]
    }});
    let info = decode(429, &body);
    assert_eq!(info.error_type.as_deref(), Some("RESOURCE_EXHAUSTED"));
    // The scheduler tells daily exhaustion apart by this code.
    assert_eq!(info.code.as_deref(), Some(CODE_DAILY_QUOTA));
    assert_eq!(CODE_DAILY_QUOTA, "daily_quota_exceeded");
    // No RetryInfo: the hint is read from the message.
    assert_eq!(info.retry_after_ms, Some(2_472_000));
}

#[test]
fn decode_retry_hint_variants() {
    let with_delay = |delay: Value| {
        decode(
            429,
            &json!({"error": {"code": 429, "message": "slow", "status": "RESOURCE_EXHAUSTED", "details": [
                {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": delay}
            ]}}),
        )
        .retry_after_ms
    };
    assert_eq!(with_delay(json!("12s")), Some(12_000));
    assert_eq!(with_delay(json!("1.5s")), Some(1_500));
    assert_eq!(with_delay(json!("0.5s")), Some(500));
    assert_eq!(with_delay(json!("0s")), Some(0));
    assert_eq!(
        with_delay(json!({"seconds": "3", "nanos": 250000000})),
        Some(3_250)
    );
    assert_eq!(with_delay(json!("soon")), None);

    let from_message = |message: &str| {
        decode(
            429,
            &json!({"error": {"code": 429, "message": message, "status": "RESOURCE_EXHAUSTED"}}),
        )
        .retry_after_ms
    };
    assert_eq!(
        from_message("Resource has been exhausted. Please retry in 12.3s."),
        Some(12_300)
    );
    assert_eq!(from_message("Please retry in 850ms."), Some(850));
    assert_eq!(
        from_message("Resource exhausted, please try again later."),
        None
    );

    // ErrorInfo metadata, as some Google backends report it.
    let quota_reset = decode(
        429,
        &json!({"error": {"code": 429, "message": "exhausted", "status": "RESOURCE_EXHAUSTED", "details": [
            {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "QUOTA_EXHAUSTED",
             "metadata": {"quotaResetDelay": "1h2m3s"}}
        ]}}),
    );
    assert_eq!(quota_reset.retry_after_ms, Some(3_723_000));
    assert_eq!(quota_reset.code.as_deref(), Some("QUOTA_EXHAUSTED"));
}

#[test]
fn decode_invalid_api_key() {
    // Gemini answers a bad key with 400 INVALID_ARGUMENT; only the detail
    // reason says it is an authentication problem.
    let body = json!({"error": {
        "code": 400,
        "message": "API key not valid. Please pass a valid API key.",
        "status": "INVALID_ARGUMENT",
        "details": [
            {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "API_KEY_INVALID", "domain": "googleapis.com",
             "metadata": {"service": "generativelanguage.googleapis.com"}},
            {"@type": "type.googleapis.com/google.rpc.LocalizedMessage", "locale": "en-US", "message": "API key not valid. Please pass a valid API key."}
        ]
    }});
    assert_eq!(
        decode(400, &body),
        UpstreamErrorInfo {
            message: "API key not valid. Please pass a valid API key.".into(),
            error_type: Some("INVALID_ARGUMENT".into()),
            code: Some("API_KEY_INVALID".into()),
            retry_after_ms: None,
        }
    );
}

#[test]
fn decode_auth_permission_and_not_found() {
    let unauthenticated = json!({"error": {
        "code": 401,
        "message": "Request had invalid authentication credentials. Expected OAuth 2 access token, login cookie or other valid authentication credential.",
        "status": "UNAUTHENTICATED"
    }});
    let info = decode(401, &unauthenticated);
    assert_eq!(info.error_type.as_deref(), Some("UNAUTHENTICATED"));
    assert_eq!(info.code, None);

    let denied = json!({"error": {"code": 403, "message": "Your API key was reported as leaked. Please use another API key.", "status": "PERMISSION_DENIED"}});
    assert_eq!(
        decode(403, &denied).error_type.as_deref(),
        Some("PERMISSION_DENIED")
    );

    let missing = json!({"error": {
        "code": 404,
        "message": "models/gemini-9 is not found for API version v1beta, or is not supported for generateContent.",
        "status": "NOT_FOUND"
    }});
    let info = decode(404, &missing);
    assert_eq!(info.error_type.as_deref(), Some("NOT_FOUND"));
    assert!(info.message.contains("gemini-9"));
}

#[test]
fn decode_invalid_request_and_overloaded() {
    let invalid = json!({"error": {
        "code": 400,
        "message": "Invalid JSON payload received. Unknown name \"foo\": Cannot find field.",
        "status": "INVALID_ARGUMENT",
        "details": [{"@type": "type.googleapis.com/google.rpc.BadRequest", "fieldViolations": [{"description": "Invalid JSON payload received. Unknown name \"foo\": Cannot find field."}]}]
    }});
    assert_eq!(
        decode(400, &invalid),
        UpstreamErrorInfo {
            message: "Invalid JSON payload received. Unknown name \"foo\": Cannot find field."
                .into(),
            error_type: Some("INVALID_ARGUMENT".into()),
            code: None,
            retry_after_ms: None,
        }
    );
    let overloaded = json!({"error": {"code": 503, "message": "The model is overloaded. Please try again later.", "status": "UNAVAILABLE"}});
    assert_eq!(
        decode(503, &overloaded),
        UpstreamErrorInfo {
            message: "The model is overloaded. Please try again later.".into(),
            error_type: Some("UNAVAILABLE".into()),
            code: None,
            retry_after_ms: None,
        }
    );
    let internal = json!({"error": {"code": 500, "message": "An internal error has occurred.", "status": "INTERNAL"}});
    assert_eq!(
        decode(500, &internal).error_type.as_deref(),
        Some("INTERNAL")
    );
    let deadline = json!({"error": {"code": 504, "message": "Deadline expired before operation could complete.", "status": "DEADLINE_EXCEEDED"}});
    assert_eq!(
        decode(504, &deadline).error_type.as_deref(),
        Some("DEADLINE_EXCEEDED")
    );
}

#[test]
fn decode_array_wrapped_error() {
    // The JSON-array stream form wraps the envelope in an array.
    let body = json!([{"error": {
        "code": 429,
        "message": "Resource has been exhausted (e.g. check quota).",
        "status": "RESOURCE_EXHAUSTED",
        "details": [{"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "7s"}]
    }}]);
    assert_eq!(
        decode(429, &body),
        UpstreamErrorInfo {
            message: "Resource has been exhausted (e.g. check quota).".into(),
            error_type: Some("RESOURCE_EXHAUSTED".into()),
            code: None,
            retry_after_ms: Some(7_000),
        }
    );
}

#[test]
fn decode_other_vendors_envelopes() {
    // Claude on Vertex answers model-level failures in Anthropic's shape.
    let anthropic =
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
    assert_eq!(
        decode(529, &anthropic),
        UpstreamErrorInfo {
            message: "Overloaded".into(),
            error_type: Some("overloaded_error".into()),
            code: None,
            retry_after_ms: None,
        }
    );
    // OpenAI-compatible relays.
    let openai = json!({"error": {"message": "Rate limit reached. Please try again in 6s.", "type": "rate_limit_error", "code": "rate_limit_exceeded"}});
    assert_eq!(
        decode(429, &openai),
        UpstreamErrorInfo {
            message: "Rate limit reached. Please try again in 6s.".into(),
            error_type: Some("rate_limit_error".into()),
            code: Some("rate_limit_exceeded".into()),
            retry_after_ms: Some(6_000),
        }
    );
    let string_error = json!({"error": "upstream is down"});
    assert_eq!(decode(502, &string_error).message, "upstream is down");
    let flat = json!({"message": "no healthy upstream", "code": 503});
    assert_eq!(decode(503, &flat).message, "no healthy upstream");
    // An envelope without a message still says something.
    let bare = json!({"error": {"code": 500, "status": "INTERNAL"}});
    assert_eq!(decode(500, &bare).message, "INTERNAL");
}

// ---------------------------------------------------------------------------
// decode_error: bodies that are not JSON
// ---------------------------------------------------------------------------

#[test]
fn decode_html_plain_text_and_empty_bodies() {
    let html = b"<!DOCTYPE html>\n<html lang=en>\n  <title>Error 502 (Server Error)!!1</title>\n  <p>The server encountered a temporary error.</p></html>";
    assert_eq!(
        GeminiCodec.decode_error(502, html),
        UpstreamErrorInfo {
            message: "upstream returned HTTP 502: Error 502 (Server Error)!!1".into(),
            ..UpstreamErrorInfo::default()
        }
    );
    let untitled = b"<html><body><h1>Service Unavailable</h1></body></html>";
    assert_eq!(
        GeminiCodec.decode_error(503, untitled).message,
        "upstream returned HTTP 503 with an HTML error page"
    );

    let plain = GeminiCodec.decode_error(
        503,
        b"  upstream connect error or disconnect/reset before headers. retry after 3s  ",
    );
    assert_eq!(
        plain.message,
        "upstream connect error or disconnect/reset before headers. retry after 3s"
    );
    assert_eq!(plain.retry_after_ms, Some(3_000));
    assert_eq!(plain.error_type, None);

    for empty in [&b""[..], b"   \n"] {
        assert_eq!(
            GeminiCodec.decode_error(500, empty),
            UpstreamErrorInfo {
                message: "upstream returned HTTP 500 with an empty body".into(),
                ..UpstreamErrorInfo::default()
            }
        );
    }

    // Not UTF-8, valid JSON that is not an envelope, and very long text.
    assert!(
        !GeminiCodec
            .decode_error(500, &[0xff, 0xfe, 0x00])
            .message
            .is_empty()
    );
    assert_eq!(
        GeminiCodec.decode_error(500, b"[1, 2, 3]").message,
        "[1, 2, 3]"
    );
    assert_eq!(
        GeminiCodec.decode_error(500, b"\"just a string\"").message,
        "\"just a string\""
    );
    let long = "x".repeat(10_000);
    let truncated = GeminiCodec.decode_error(500, long.as_bytes()).message;
    assert_eq!(truncated.chars().count(), 2001);
    assert!(truncated.ends_with('…'));
}
