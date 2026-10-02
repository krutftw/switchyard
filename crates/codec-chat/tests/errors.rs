//! `encode_error` for every error kind and `decode_error` for real vendor
//! error bodies.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::time::Duration;
use switchyard_codec_chat::ChatCodec;
use switchyard_core::codec::Codec;
use switchyard_core::error::{ApiError, ErrorKind, UpstreamErrorInfo};

fn decode(status: u16, body: &str) -> UpstreamErrorInfo {
    ChatCodec.decode_error(status, body.as_bytes())
}

// ---------------------------------------------------------------------------
// encode_error
// ---------------------------------------------------------------------------

#[test]
fn encode_error_for_every_kind() {
    let cases = [
        (
            ErrorKind::InvalidRequest,
            "invalid_request_error",
            Value::Null,
        ),
        (
            ErrorKind::Authentication,
            "authentication_error",
            json!("invalid_api_key"),
        ),
        (ErrorKind::Permission, "permission_error", Value::Null),
        (
            ErrorKind::NotFound,
            "invalid_request_error",
            json!("not_found"),
        ),
        (
            ErrorKind::TooLarge,
            "invalid_request_error",
            json!("request_too_large"),
        ),
        (
            ErrorKind::RateLimit,
            "rate_limit_error",
            json!("rate_limit_exceeded"),
        ),
        (ErrorKind::Upstream, "server_error", json!("upstream_error")),
        (
            ErrorKind::Unavailable,
            "server_error",
            json!("service_unavailable"),
        ),
        (ErrorKind::Timeout, "server_error", json!("request_timeout")),
        (
            ErrorKind::Internal,
            "server_error",
            json!("internal_server_error"),
        ),
    ];
    for (kind, wire_type, code) in cases {
        let body = ChatCodec.encode_error(&ApiError::new(kind, "something happened"));
        assert_eq!(
            body,
            json!({"error": {
                "message": "something happened",
                "type": wire_type,
                "param": null,
                "code": code
            }}),
            "{kind:?}"
        );
    }
}

#[test]
fn encode_error_uses_the_errors_own_code_and_param() {
    let body = ChatCodec.encode_error(&ApiError::unknown_model("gpt-9"));
    assert_eq!(
        body,
        json!({"error": {
            "message": "unknown model `gpt-9`: no configured provider serves it",
            "type": "invalid_request_error",
            "param": "model",
            "code": "model_not_found"
        }})
    );
    // The four members are always present, in OpenAI's order.
    assert_eq!(
        body["error"]
            .as_object()
            .expect("object")
            .keys()
            .collect::<Vec<_>>(),
        ["message", "type", "param", "code"]
    );

    let error = ApiError::invalid_request("`messages` must be an array")
        .with_param("messages")
        .with_retry_after(Duration::from_secs(3));
    assert_eq!(
        ChatCodec.encode_error(&error)["error"],
        json!({
            "message": "`messages` must be an array",
            "type": "invalid_request_error",
            "param": "messages",
            "code": null
        })
    );
}

// ---------------------------------------------------------------------------
// decode_error: OpenAI
// ---------------------------------------------------------------------------

#[test]
fn openai_rate_limit_with_retry_hint() {
    let info = decode(
        429,
        r#"{
  "error": {
    "message": "Rate limit reached for gpt-4o in organization org-abc on tokens per min (TPM): Limit 30000, Used 29500, Requested 1200. Please try again in 1.4s. Visit https://platform.openai.com/account/rate-limits to learn more.",
    "type": "tokens",
    "param": null,
    "code": "rate_limit_exceeded"
  }
}"#,
    );
    assert!(info.message.starts_with("Rate limit reached for gpt-4o"));
    assert_eq!(info.error_type.as_deref(), Some("tokens"));
    assert_eq!(info.code.as_deref(), Some("rate_limit_exceeded"));
    assert_eq!(info.retry_after_ms, Some(1400));
}

#[test]
fn openai_quota_exhausted() {
    let info = decode(
        429,
        r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details. For more information on this error, read the docs: https://platform.openai.com/docs/guides/error-codes/api-errors.","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#,
    );
    assert_eq!(info.error_type.as_deref(), Some("insufficient_quota"));
    assert_eq!(info.code.as_deref(), Some("insufficient_quota"));
    // Waiting does not help: there is no hint to find.
    assert_eq!(info.retry_after_ms, None);
}

#[test]
fn openai_invalid_api_key() {
    let info = decode(
        401,
        r#"{"error":{"message":"Incorrect API key provided: sk-proj-****abcd. You can find your API key at https://platform.openai.com/account/api-keys.","type":"invalid_request_error","param":null,"code":"invalid_api_key"}}"#,
    );
    assert!(info.message.starts_with("Incorrect API key provided"));
    assert_eq!(info.error_type.as_deref(), Some("invalid_request_error"));
    assert_eq!(info.code.as_deref(), Some("invalid_api_key"));
    assert_eq!(info.retry_after_ms, None);
}

#[test]
fn openai_invalid_request() {
    let info = decode(
        400,
        r#"{"error":{"message":"Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.","type":"invalid_request_error","param":"max_tokens","code":"unsupported_parameter"}}"#,
    );
    assert_eq!(
        info,
        UpstreamErrorInfo {
            message: "Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.".into(),
            error_type: Some("invalid_request_error".into()),
            code: Some("unsupported_parameter".into()),
            retry_after_ms: None,
        }
    );
}

#[test]
fn openai_overloaded() {
    let info = decode(
        503,
        r#"{"error":{"message":"The engine is currently overloaded, please try again later.","type":"server_error","param":null,"code":"server_is_overloaded"}}"#,
    );
    assert_eq!(info.error_type.as_deref(), Some("server_error"));
    assert_eq!(info.code.as_deref(), Some("server_is_overloaded"));
    assert_eq!(info.retry_after_ms, None);
}

// ---------------------------------------------------------------------------
// decode_error: compatible vendors
// ---------------------------------------------------------------------------

#[test]
fn groq_rate_limit_with_compound_duration() {
    let info = decode(
        429,
        r#"{"error":{"message":"Rate limit reached for model `llama-3.3-70b-versatile` in organization `org_01` service tier `on_demand` on tokens per day (TPD): Limit 100000, Used 99000, Requested 2000. Please try again in 7m12.5s. Need more tokens? Upgrade to Dev Tier today.","type":"tokens","code":"rate_limit_exceeded"}}"#,
    );
    assert_eq!(info.code.as_deref(), Some("rate_limit_exceeded"));
    assert_eq!(info.retry_after_ms, Some(432_500));
}

#[test]
fn openrouter_relayed_provider_error() {
    let info = decode(
        429,
        r#"{"error":{"message":"Provider returned error","code":429,"metadata":{"raw":"google/gemini-2.0-flash-exp:free is temporarily rate-limited upstream. Please retry shortly.","provider_name":"Google"}},"user_id":"user_2x"}"#,
    );
    assert_eq!(
        info.message,
        "Provider returned error (google/gemini-2.0-flash-exp:free is temporarily rate-limited upstream. Please retry shortly.)"
    );
    assert_eq!(info.error_type, None);
    assert_eq!(info.code.as_deref(), Some("429"));
}

#[test]
fn deepseek_insufficient_balance() {
    let info = decode(
        402,
        r#"{"error":{"message":"Insufficient Balance","type":"unknown_error","param":null,"code":"invalid_request_error"}}"#,
    );
    assert_eq!(info.message, "Insufficient Balance");
    assert_eq!(info.error_type.as_deref(), Some("unknown_error"));
    assert_eq!(info.code.as_deref(), Some("invalid_request_error"));
}

#[test]
fn google_compatible_endpoint_array_envelope_with_retry_info() {
    let info = decode(
        429,
        r#"[{
  "error": {
    "code": 429,
    "message": "You exceeded your current quota, please check your plan and billing details.",
    "status": "RESOURCE_EXHAUSTED",
    "details": [
      {"@type": "type.googleapis.com/google.rpc.Help", "links": [{"description": "Learn more", "url": "https://ai.google.dev/gemini-api/docs/rate-limits"}]},
      {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "34s"}
    ]
  }
}]"#,
    );
    assert_eq!(info.error_type.as_deref(), Some("RESOURCE_EXHAUSTED"));
    assert_eq!(info.code.as_deref(), Some("429"));
    assert_eq!(info.retry_after_ms, Some(34_000));
}

#[test]
fn vllm_and_fastapi_shapes() {
    let info = decode(
        400,
        r#"{"object":"error","message":"This model's maximum context length is 4096 tokens. However, you requested 5000 tokens.","type":"BadRequestError","param":null,"code":400}"#,
    );
    assert!(
        info.message
            .starts_with("This model's maximum context length")
    );
    assert_eq!(info.error_type.as_deref(), Some("BadRequestError"));
    assert_eq!(info.code.as_deref(), Some("400"));

    assert_eq!(
        decode(404, r#"{"detail":"Not Found"}"#).message,
        "Not Found"
    );

    // FastAPI validation errors: `detail` is a list.
    let info = decode(
        422,
        r#"{"detail":[{"loc":["body","messages"],"msg":"field required","type":"value_error.missing"}]}"#,
    );
    assert!(info.message.contains("field required"));
}

#[test]
fn string_error_member_and_numeric_retry_fields() {
    let info = decode(500, r#"{"error":"model runner has unexpectedly stopped"}"#);
    assert_eq!(info.message, "model runner has unexpectedly stopped");

    let info = decode(
        429,
        r#"{"error":{"message":"slow down","type":"rate_limit_error","retry_after":2.5}}"#,
    );
    assert_eq!(info.retry_after_ms, Some(2500));

    let info = decode(
        429,
        r#"{"error":{"message":"slow down"},"retry_after_ms":750}"#,
    );
    assert_eq!(info.retry_after_ms, Some(750));

    // An error object without a message still says something.
    let info = decode(500, r#"{"error":{"code":"engine_overloaded"}}"#);
    assert_eq!(info.message, "engine_overloaded");
}

// ---------------------------------------------------------------------------
// decode_error: not JSON
// ---------------------------------------------------------------------------

#[test]
fn html_error_pages() {
    let info = decode(
        502,
        "<html>\r\n<head><title>502 Bad Gateway</title></head>\r\n<body>\r\n<center><h1>502 Bad Gateway</h1></center>\r\n<hr><center>cloudflare</center>\r\n</body>\r\n</html>\r\n",
    );
    assert_eq!(
        info,
        UpstreamErrorInfo {
            message: "upstream returned HTTP 502: 502 Bad Gateway".into(),
            error_type: None,
            code: None,
            retry_after_ms: None,
        }
    );
    let info = decode(
        403,
        "<!DOCTYPE html><html><body><h1>Access denied</h1></body></html>",
    );
    assert_eq!(
        info.message,
        "upstream returned HTTP 403 with an HTML error page"
    );
}

#[test]
fn plain_text_bodies() {
    let info = decode(
        503,
        "upstream connect error or disconnect/reset before headers. reset reason: connection termination",
    );
    assert_eq!(
        info.message,
        "upstream connect error or disconnect/reset before headers. reset reason: connection termination"
    );
    assert_eq!(info.retry_after_ms, None);

    let info = decode(429, "Too many requests. Retry after 30 seconds.\n");
    assert_eq!(info.message, "Too many requests. Retry after 30 seconds.");
    assert_eq!(info.retry_after_ms, Some(30_000));
}

#[test]
fn empty_and_unusual_bodies() {
    assert_eq!(
        decode(500, "").message,
        "upstream returned HTTP 500 with an empty body"
    );
    assert_eq!(
        decode(504, "  \r\n ").message,
        "upstream returned HTTP 504 with an empty body"
    );

    // JSON that is not an error envelope is reported as it is.
    assert_eq!(decode(500, "null").message, "null");
    assert_eq!(
        decode(500, r#"{"unexpected":true}"#).message,
        r#"{"unexpected":true}"#
    );

    // Invalid UTF-8 does not panic.
    let info = ChatCodec.decode_error(500, &[0xff, 0xfe, b'o', b'o', b'p', b's']);
    assert!(info.message.ends_with("oops"));

    // Huge bodies are truncated.
    let info = decode(500, &"x".repeat(10_000));
    assert_eq!(info.message.chars().count(), 2001);
    assert!(info.message.ends_with('…'));
}
