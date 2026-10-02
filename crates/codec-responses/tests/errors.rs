//! `encode_error` for every error kind and `decode_error` for the bodies
//! upstreams actually send.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::time::Duration;
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::{ApiError, Codec, ErrorKind, UpstreamErrorInfo};

fn encode(error: &ApiError) -> Value {
    ResponsesCodec.encode_error(error)
}

fn decode(status: u16, body: &str) -> UpstreamErrorInfo {
    ResponsesCodec.decode_error(status, body.as_bytes())
}

// ---------------------------------------------------------------------------
// encode_error
// ---------------------------------------------------------------------------

#[test]
fn encode_error_for_every_error_kind() {
    let cases = [
        (
            ErrorKind::InvalidRequest,
            400,
            "invalid_request_error",
            Value::Null,
        ),
        (
            ErrorKind::Authentication,
            401,
            "authentication_error",
            json!("invalid_api_key"),
        ),
        (ErrorKind::Permission, 403, "permission_error", Value::Null),
        (
            ErrorKind::NotFound,
            404,
            "invalid_request_error",
            Value::Null,
        ),
        (
            ErrorKind::TooLarge,
            413,
            "invalid_request_error",
            json!("request_too_large"),
        ),
        (
            ErrorKind::RateLimit,
            429,
            "rate_limit_error",
            json!("rate_limit_exceeded"),
        ),
        (
            ErrorKind::Internal,
            500,
            "server_error",
            json!("internal_server_error"),
        ),
        (
            ErrorKind::Upstream,
            502,
            "server_error",
            json!("upstream_error"),
        ),
        (
            ErrorKind::Unavailable,
            503,
            "service_unavailable_error",
            json!("service_unavailable"),
        ),
        (
            ErrorKind::Timeout,
            504,
            "server_error",
            json!("request_timeout"),
        ),
    ];
    for (kind, status, wire_type, code) in cases {
        let error = ApiError::new(kind, "Something went wrong.");
        assert_eq!(error.status, status);
        assert_eq!(
            encode(&error),
            json!({"error": {
                "message": "Something went wrong.",
                "type": wire_type,
                "param": null,
                "code": code
            }}),
            "{kind:?}"
        );
    }
}

#[test]
fn encode_error_carries_code_and_param() {
    assert_eq!(
        encode(&ApiError::unknown_model("gpt-9")),
        json!({"error": {
            "message": "unknown model `gpt-9`: no configured provider serves it",
            "type": "invalid_request_error",
            "param": "model",
            "code": "model_not_found"
        }})
    );
    // An explicit code wins over the kind's default; retry hints are a
    // header, not part of the body.
    let limited = ApiError::rate_limit("Out of quota.")
        .with_code("insufficient_quota")
        .with_retry_after(Duration::from_secs(30));
    assert_eq!(
        encode(&limited),
        json!({"error": {
            "message": "Out of quota.",
            "type": "rate_limit_error",
            "param": null,
            "code": "insufficient_quota"
        }})
    );
    let invalid =
        ApiError::invalid_request("`input` must be a string or an array").with_param("input");
    assert_eq!(encode(&invalid)["error"]["param"], json!("input"));
}

// ---------------------------------------------------------------------------
// decode_error: vendor bodies
// ---------------------------------------------------------------------------

#[test]
fn decode_error_rate_limit_with_retry_hint() {
    let info = decode(
        429,
        r#"{
  "error": {
    "message": "Rate limit reached for gpt-5 in organization org-abc on tokens per min (TPM): Limit 30000, Used 28500, Requested 2100. Please try again in 1.2s. Visit https://platform.openai.com/account/rate-limits to learn more.",
    "type": "tokens",
    "param": null,
    "code": "rate_limit_exceeded"
  }
}"#,
    );
    assert!(info.message.starts_with("Rate limit reached for gpt-5"));
    assert_eq!(info.error_type.as_deref(), Some("tokens"));
    assert_eq!(info.code.as_deref(), Some("rate_limit_exceeded"));
    assert_eq!(info.retry_after_ms, Some(1200));
}

#[test]
fn decode_error_quota_has_no_retry_hint() {
    let info = decode(
        429,
        r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details. For more information on this error, read the docs: https://platform.openai.com/docs/guides/error-codes/api-errors.","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#,
    );
    assert_eq!(info.error_type.as_deref(), Some("insufficient_quota"));
    assert_eq!(info.code.as_deref(), Some("insufficient_quota"));
    assert_eq!(info.retry_after_ms, None);
}

#[test]
fn decode_error_auth() {
    let info = decode(
        401,
        r#"{"error":{"message":"Incorrect API key provided: sk-proj-********************abcd. You can find your API key at https://platform.openai.com/account/api-keys.","type":"invalid_request_error","param":null,"code":"invalid_api_key"}}"#,
    );
    assert!(info.message.starts_with("Incorrect API key provided"));
    assert_eq!(info.error_type.as_deref(), Some("invalid_request_error"));
    assert_eq!(info.code.as_deref(), Some("invalid_api_key"));
}

#[test]
fn decode_error_invalid_request() {
    let info = decode(
        400,
        r#"{"error":{"message":"Unsupported parameter: 'temperature' is not supported with this model.","type":"invalid_request_error","param":"temperature","code":"unsupported_parameter"}}"#,
    );
    assert_eq!(
        info,
        UpstreamErrorInfo {
            message: "Unsupported parameter: 'temperature' is not supported with this model."
                .into(),
            error_type: Some("invalid_request_error".into()),
            code: Some("unsupported_parameter".into()),
            retry_after_ms: None,
        }
    );
}

#[test]
fn decode_error_overloaded() {
    let info = decode(
        503,
        r#"{"error":{"message":"The server is currently overloaded with other requests. Please retry after 20 seconds.","type":"service_unavailable_error","param":null,"code":"server_is_overloaded"}}"#,
    );
    assert_eq!(
        info.error_type.as_deref(),
        Some("service_unavailable_error")
    );
    assert_eq!(info.code.as_deref(), Some("server_is_overloaded"));
    assert_eq!(info.retry_after_ms, Some(20_000));
}

#[test]
fn decode_error_retry_hint_spellings() {
    let hint = |message: &str| {
        decode(429, &json!({"error": {"message": message}}).to_string()).retry_after_ms
    };
    assert_eq!(hint("Please try again in 1.242s."), Some(1242));
    assert_eq!(hint("Please try again in 120ms."), Some(120));
    assert_eq!(hint("Please try again in 6m0s."), Some(360_000));
    assert_eq!(
        hint("Limit reached. Try again in 2 minutes."),
        Some(120_000)
    );
    assert_eq!(hint("Please retry after 20 seconds"), Some(20_000));
    assert_eq!(hint("Please try again later."), None);

    // Numeric fields next to the message.
    let field = |body: Value| decode(429, &body.to_string()).retry_after_ms;
    assert_eq!(
        field(json!({"error": {"message": "slow down", "retry_after": 12}})),
        Some(12_000)
    );
    assert_eq!(
        field(json!({"error": {"message": "slow down"}, "retry_after_ms": 750})),
        Some(750)
    );
    assert_eq!(
        field(
            json!({"error": {"message": "limit", "type": "usage_limit_reached", "resets_in_seconds": 3600}})
        ),
        Some(3_600_000)
    );
}

// ---------------------------------------------------------------------------
// decode_error: other shapes
// ---------------------------------------------------------------------------

#[test]
fn decode_error_from_compatible_servers() {
    // Numeric code (vLLM), string error (Ollama), FastAPI `detail`, a bare
    // message, a Google-style array wrapper, an in-stream failed response.
    let vllm = decode(
        400,
        r#"{"error":{"message":"max_tokens is too large","type":"BadRequestError","param":null,"code":400}}"#,
    );
    assert_eq!(vllm.code.as_deref(), Some("400"));
    assert_eq!(vllm.error_type.as_deref(), Some("BadRequestError"));

    let ollama = decode(404, r#"{"error":"model 'llama9' not found"}"#);
    assert_eq!(ollama.message, "model 'llama9' not found");
    assert_eq!((ollama.error_type, ollama.code), (None, None));

    assert_eq!(
        decode(404, r#"{"detail":"Not Found"}"#).message,
        "Not Found"
    );
    assert_eq!(
        decode(500, r#"{"message":"internal failure","code":"E_FAIL"}"#)
            .code
            .as_deref(),
        Some("E_FAIL")
    );

    let google = decode(
        429,
        r#"[{"error":{"code":429,"message":"Quota exceeded for quota metric","status":"RESOURCE_EXHAUSTED"}}]"#,
    );
    assert_eq!(google.message, "Quota exceeded for quota metric");
    assert_eq!(google.error_type.as_deref(), Some("RESOURCE_EXHAUSTED"));
    assert_eq!(google.code.as_deref(), Some("429"));

    let failed = decode(
        200,
        r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"The model failed."}}}"#,
    );
    assert_eq!(failed.message, "The model failed.");
    assert_eq!(failed.code.as_deref(), Some("server_error"));

    // An error object without a message still says something.
    let codeless = decode(
        400,
        r#"{"error":{"type":"invalid_request_error","code":"bad_thing"}}"#,
    );
    assert_eq!(codeless.message, "bad_thing");
}

#[test]
fn decode_error_html_plain_text_and_empty_bodies() {
    let html = decode(
        502,
        "<html>\r\n<head><title>502 Bad Gateway</title></head>\r\n<body>\r\n<center><h1>502 Bad Gateway</h1></center>\r\n<hr><center>cloudflare</center>\r\n</body>\r\n</html>\r\n",
    );
    assert_eq!(html.message, "upstream returned HTTP 502: 502 Bad Gateway");
    assert_eq!(
        (html.error_type, html.code, html.retry_after_ms),
        (None, None, None)
    );

    let untitled = decode(
        503,
        "<!DOCTYPE html><html><body><h1>Service Unavailable</h1></body></html>",
    );
    assert_eq!(
        untitled.message,
        "upstream returned HTTP 503 with an HTML error page"
    );

    let plain = decode(
        503,
        "upstream connect error or disconnect/reset before headers. retry after 5s\n",
    );
    assert_eq!(
        plain.message,
        "upstream connect error or disconnect/reset before headers. retry after 5s"
    );
    assert_eq!(plain.retry_after_ms, Some(5000));

    assert_eq!(
        decode(504, "").message,
        "upstream returned HTTP 504 with an empty body"
    );
    assert_eq!(
        decode(504, "  \n ").message,
        "upstream returned HTTP 504 with an empty body"
    );

    // JSON that is not an error envelope is reported as text.
    assert_eq!(
        decode(500, r#"{"status":"sad"}"#).message,
        r#"{"status":"sad"}"#
    );
}

#[test]
fn decode_error_never_fails_on_garbage() {
    let info = ResponsesCodec.decode_error(500, &[0xff, 0xfe, b'o', b'o', b'p', b's']);
    assert!(info.message.ends_with("oops"));

    let long = "x".repeat(10_000);
    let info = decode(500, &long);
    assert_eq!(info.message.chars().count(), 2001);
    assert!(info.message.ends_with('…'));

    let long_json = json!({"error": {"message": "y".repeat(10_000)}}).to_string();
    assert_eq!(decode(500, &long_json).message.chars().count(), 2001);
}
