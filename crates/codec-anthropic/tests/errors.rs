//! The error envelope: rendering `ApiError` and reading upstream error bodies.

use pretty_assertions::assert_eq;
use serde_json::json;
use switchyard_codec_anthropic::AnthropicCodec;
use switchyard_core::{ApiError, Codec, ErrorKind, FailureClass, UpstreamError, UpstreamErrorInfo};

fn decode(status: u16, body: &str) -> UpstreamErrorInfo {
    AnthropicCodec.decode_error(status, body.as_bytes())
}

// ---------------------------------------------------------------------------
// encode_error
// ---------------------------------------------------------------------------

#[test]
fn encode_error_for_every_kind() {
    for (kind, expected_type) in [
        (ErrorKind::InvalidRequest, "invalid_request_error"),
        (ErrorKind::Authentication, "authentication_error"),
        (ErrorKind::Permission, "permission_error"),
        (ErrorKind::NotFound, "not_found_error"),
        (ErrorKind::TooLarge, "request_too_large"),
        (ErrorKind::RateLimit, "rate_limit_error"),
        (ErrorKind::Upstream, "api_error"),
        (ErrorKind::Unavailable, "overloaded_error"),
        (ErrorKind::Timeout, "timeout_error"),
        (ErrorKind::Internal, "api_error"),
    ] {
        let body = AnthropicCodec.encode_error(&ApiError::new(kind, "something went wrong"));
        assert_eq!(
            body,
            json!({"type": "error", "error": {"type": expected_type, "message": "something went wrong"}}),
            "{kind:?}"
        );
    }
}

#[test]
fn encode_error_statuses_with_a_type_of_their_own() {
    let billing = ApiError::permission("Your credit balance is too low").with_status(402);
    assert_eq!(
        AnthropicCodec.encode_error(&billing)["error"]["type"],
        json!("billing_error")
    );
    let conflict = ApiError::invalid_request("conflict").with_status(409);
    assert_eq!(
        AnthropicCodec.encode_error(&conflict)["error"]["type"],
        json!("conflict_error")
    );
    let overloaded = ApiError::upstream("Overloaded").with_status(529);
    assert_eq!(
        AnthropicCodec.encode_error(&overloaded)["error"]["type"],
        json!("overloaded_error")
    );
    // An upstream 413 reaches the client as a request fault that keeps its
    // status; the type follows the status (notes 02 section 10.2).
    let mut too_large = UpstreamError::transport("Request exceeds the maximum allowed size");
    too_large.status = 413;
    too_large.class = FailureClass::Request;
    let relayed = too_large.to_api_error();
    assert_eq!(
        (relayed.status, relayed.kind),
        (413, ErrorKind::InvalidRequest)
    );
    assert_eq!(
        AnthropicCodec.encode_error(&relayed)["error"]["type"],
        json!("request_too_large")
    );
    // A request fault with any other status stays an invalid request.
    let unprocessable = ApiError::invalid_request("bad").with_status(422);
    assert_eq!(
        AnthropicCodec.encode_error(&unprocessable)["error"]["type"],
        json!("invalid_request_error")
    );
}

#[test]
fn encode_error_envelope_has_exactly_the_vendor_fields() {
    let error = ApiError::unknown_model("nope").with_retry_after(std::time::Duration::from_secs(3));
    let body = AnthropicCodec.encode_error(&error);
    // code, param and retry hints have no place in the Anthropic envelope.
    assert_eq!(
        body,
        json!({"type": "error", "error": {
            "type": "not_found_error",
            "message": "unknown model `nope`: no configured provider serves it"
        }})
    );
}

// ---------------------------------------------------------------------------
// decode_error: Anthropic bodies
// ---------------------------------------------------------------------------

#[test]
fn decode_error_rate_limit() {
    let info = decode(
        429,
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed the rate limit for your organization (0b5e7a1c) of 30,000 input tokens per minute. For details, refer to: https://docs.claude.com/en/api/rate-limits. You can see the response headers for current usage. Please reduce the prompt length or the maximum tokens requested, or try again later."},"request_id":"req_011CSHoEeqs5C35K2UUqR7Fy"}"#,
    );
    assert_eq!(info.error_type.as_deref(), Some("rate_limit_error"));
    assert!(
        info.message
            .starts_with("This request would exceed the rate limit")
    );
    assert_eq!(info.code, None);
    // "try again later" is not a duration.
    assert_eq!(info.retry_after_ms, None);
}

#[test]
fn decode_error_quota_and_billing() {
    // Monthly spend cap: a 429 that will not go away by waiting a minute.
    let info = decode(
        429,
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"You have reached your monthly spend limit.","details":{"error_code":"enforced_spend_limit_reached"}},"request_id":"req_1"}"#,
    );
    assert_eq!(info.error_type.as_deref(), Some("rate_limit_error"));
    assert_eq!(info.code.as_deref(), Some("enforced_spend_limit_reached"));
    // User-set usage limit: reported as a 400.
    let info = decode(
        400,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"You have reached your specified API usage limits. You will regain access on 2026-11-01 at 00:00 UTC."},"request_id":"req_2"}"#,
    );
    assert_eq!(info.error_type.as_deref(), Some("invalid_request_error"));
    assert!(
        info.message
            .starts_with("You have reached your specified API usage limits")
    );
    // No credit.
    let info = decode(
        402,
        r#"{"type":"error","error":{"type":"billing_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."}}"#,
    );
    assert_eq!(info.error_type.as_deref(), Some("billing_error"));
}

#[test]
fn decode_error_authentication() {
    let info = decode(
        401,
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"},"request_id":"req_011CSHoEeqs5C35K2UUqR7Fy"}"#,
    );
    assert_eq!(
        info,
        UpstreamErrorInfo {
            message: "invalid x-api-key".into(),
            error_type: Some("authentication_error".into()),
            code: None,
            retry_after_ms: None,
        }
    );
}

#[test]
fn decode_error_invalid_request() {
    let info = decode(
        400,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0.thinking.signature: Invalid `signature` in `thinking` block"},"request_id":"req_3"}"#,
    );
    assert_eq!(info.error_type.as_deref(), Some("invalid_request_error"));
    assert_eq!(
        info.message,
        "messages.1.content.0.thinking.signature: Invalid `signature` in `thinking` block"
    );
}

#[test]
fn decode_error_overloaded() {
    let info = decode(
        529,
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    );
    assert_eq!(
        info,
        UpstreamErrorInfo {
            message: "Overloaded".into(),
            error_type: Some("overloaded_error".into()),
            code: None,
            retry_after_ms: None,
        }
    );
}

#[test]
fn decode_error_sse_framed_body() {
    // Some proxies answer an error status with the stream framing.
    let info = decode(
        529,
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    );
    assert_eq!(info.error_type.as_deref(), Some("overloaded_error"));
    assert_eq!(info.message, "Overloaded");
}

// ---------------------------------------------------------------------------
// decode_error: bodies of compatible gateways
// ---------------------------------------------------------------------------

#[test]
fn decode_error_openai_shaped_with_retry_hint() {
    let info = decode(
        429,
        r#"{"error":{"message":"Rate limit reached for gpt-5 in organization org-x on tokens per min (TPM): Limit 30000, Used 29000, Requested 2000. Please try again in 1.5s. Visit https://platform.openai.com/account/rate-limits to learn more.","type":"tokens","param":null,"code":"rate_limit_exceeded"}}"#,
    );
    assert_eq!(info.error_type.as_deref(), Some("tokens"));
    assert_eq!(info.code.as_deref(), Some("rate_limit_exceeded"));
    assert_eq!(info.retry_after_ms, Some(1500));
    let info = decode(
        429,
        r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#,
    );
    assert_eq!(info.error_type.as_deref(), Some("insufficient_quota"));
    // The code repeats the type: reported once.
    assert_eq!(info.code, None);
}

#[test]
fn decode_error_google_shaped_with_retry_info() {
    let info = decode(
        429,
        r#"{"error":{"code":429,"message":"Resource has been exhausted (e.g. check quota).","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"37s"}]}}"#,
    );
    assert_eq!(info.error_type.as_deref(), Some("RESOURCE_EXHAUSTED"));
    assert_eq!(info.code.as_deref(), Some("429"));
    assert_eq!(info.retry_after_ms, Some(37_000));
    // Wrapped in an array, as stream endpoints do.
    let info = decode(
        503,
        r#"[{"error":{"code":503,"message":"The model is overloaded.","status":"UNAVAILABLE"}}]"#,
    );
    assert_eq!(info.message, "The model is overloaded.");
    assert_eq!(info.error_type.as_deref(), Some("UNAVAILABLE"));
}

#[test]
fn decode_error_flat_json_shapes() {
    assert_eq!(
        decode(500, r#"{"error":"backend unavailable"}"#).message,
        "backend unavailable"
    );
    assert_eq!(
        decode(
            404,
            r#"{"message":"model not found","code":"model_not_found"}"#
        )
        .code
        .as_deref(),
        Some("model_not_found")
    );
    assert_eq!(
        decode(422, r#"{"detail":"Field required"}"#).message,
        "Field required"
    );
    let info = decode(
        422,
        r#"{"detail":[{"loc":["body","model"],"msg":"field required"}]}"#,
    );
    assert!(info.message.contains("field required"));
    let info = decode(
        400,
        r#"{"error":"invalid_grant","error_description":"Token expired"}"#,
    );
    assert_eq!(
        (info.message.as_str(), info.code.as_deref()),
        ("Token expired", Some("invalid_grant"))
    );
    assert_eq!(
        decode(500, r#""just a JSON string""#).message,
        "just a JSON string"
    );
    // A retry delay carried as a field.
    assert_eq!(
        decode(
            429,
            r#"{"error":{"message":"slow down","retry_after":2.5}}"#
        )
        .retry_after_ms,
        Some(2500)
    );
    assert_eq!(
        decode(429, r#"{"message":"slow down","retry_after_ms":750}"#).retry_after_ms,
        Some(750)
    );
}

#[test]
fn decode_error_json_without_any_message_falls_back_to_the_text() {
    let info = decode(500, r#"{"status":"failed","trace":"abc"}"#);
    assert_eq!(info.message, r#"{"status":"failed","trace":"abc"}"#);
    assert_eq!(info.error_type, None);
}

// ---------------------------------------------------------------------------
// decode_error: not JSON
// ---------------------------------------------------------------------------

#[test]
fn decode_error_html_body() {
    let info = decode(
        502,
        "<!DOCTYPE html>\n<html>\n<head><title>502 Bad Gateway</title></head>\n<body>\n<center><h1>502 Bad Gateway</h1></center>\n<hr><center>cloudflare</center>\n</body>\n</html>\n",
    );
    assert_eq!(
        info.message,
        "upstream returned an HTML page (HTTP 502): 502 Bad Gateway"
    );
    assert_eq!(info.error_type, None);
    let info = decode(
        413,
        "<html><body><h1>Request Entity Too Large</h1></body></html>",
    );
    assert_eq!(info.message, "upstream returned an HTML page (HTTP 413)");
}

#[test]
fn decode_error_plain_text_body() {
    let info = decode(
        503,
        "upstream connect error or disconnect/reset before headers. reset reason: overflow",
    );
    assert_eq!(
        info.message,
        "upstream connect error or disconnect/reset before headers. reset reason: overflow"
    );
    let info = decode(429, "Too many requests. Retry after 20 seconds.");
    assert_eq!(info.retry_after_ms, Some(20_000));
    // Long garbage is truncated, never dropped.
    let info = decode(500, &"x".repeat(10_000));
    assert_eq!(info.message.chars().count(), 601);
    assert!(info.message.ends_with('…'));
}

#[test]
fn decode_error_empty_and_binary_bodies() {
    assert_eq!(
        decode(504, "").message,
        "upstream returned HTTP 504 with an empty body"
    );
    assert_eq!(
        decode(500, "  \n ").message,
        "upstream returned HTTP 500 with an empty body"
    );
    let info = AnthropicCodec.decode_error(500, &[0xff, 0xfe, 0x00, 0x41]);
    assert!(!info.message.is_empty());
}
