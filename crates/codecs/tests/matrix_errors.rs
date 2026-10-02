//! Matrix (e): errors.
//!
//! * Every [`ErrorKind`], rendered by each protocol's `encode_error`, is the
//!   vendor's envelope with the vendor's type string, and reads back through
//!   the same protocol's `decode_error` with its message intact.
//! * Vendor error bodies (literals from notes 15) are understood by their
//!   own protocol's `decode_error`, including the retry hints in the body.
//! * For every ordered pair `(C, U)`: an upstream failure of `U`, turned
//!   into the client-facing error the way the gateway does it
//!   (`UpstreamError::to_api_error`), renders as a valid `C` envelope that
//!   still carries the upstream's message.

mod support;

use serde_json::{Value, json};
use support::harness::{ANTHROPIC, CHAT, Failures, GEMINI, RESPONSES, short};
use switchyard_codecs::codec;
use switchyard_core::error::{FailureClass, UpstreamError};
use switchyard_core::{ApiError, ErrorKind, Protocol};

const KINDS: [ErrorKind; 10] = [
    ErrorKind::InvalidRequest,
    ErrorKind::Authentication,
    ErrorKind::Permission,
    ErrorKind::NotFound,
    ErrorKind::TooLarge,
    ErrorKind::RateLimit,
    ErrorKind::Upstream,
    ErrorKind::Unavailable,
    ErrorKind::Timeout,
    ErrorKind::Internal,
];

/// The vendor's type string for an error kind.
///
/// * OpenAI (notes 15 §4.4, 08 §3): `invalid_request_error` for request
///   faults (including unknown models and oversized bodies),
///   `rate_limit_error` for 429, `service_unavailable_error` for 503,
///   `server_error` for the other server-side failures.
/// * Anthropic (notes 15 §5.5): the table of `error.type` by status.
/// * Gemini (notes 15 §6.6, §7.4): the `google.rpc.Code` name by status.
fn expected_type(protocol: Protocol, kind: ErrorKind) -> &'static str {
    match protocol {
        Protocol::OpenaiChat | Protocol::OpenaiResponses => match kind {
            ErrorKind::InvalidRequest | ErrorKind::NotFound | ErrorKind::TooLarge => {
                "invalid_request_error"
            }
            ErrorKind::Authentication => "authentication_error",
            ErrorKind::Permission => "permission_error",
            ErrorKind::RateLimit => "rate_limit_error",
            ErrorKind::Unavailable => "service_unavailable_error",
            ErrorKind::Upstream | ErrorKind::Timeout | ErrorKind::Internal => "server_error",
        },
        Protocol::Anthropic => match kind {
            ErrorKind::InvalidRequest => "invalid_request_error",
            ErrorKind::Authentication => "authentication_error",
            ErrorKind::Permission => "permission_error",
            ErrorKind::NotFound => "not_found_error",
            ErrorKind::TooLarge => "request_too_large",
            ErrorKind::RateLimit => "rate_limit_error",
            ErrorKind::Unavailable => "overloaded_error",
            ErrorKind::Timeout => "timeout_error",
            ErrorKind::Upstream | ErrorKind::Internal => "api_error",
        },
        Protocol::Gemini => match kind {
            ErrorKind::InvalidRequest | ErrorKind::TooLarge => "INVALID_ARGUMENT",
            ErrorKind::Authentication => "UNAUTHENTICATED",
            ErrorKind::Permission => "PERMISSION_DENIED",
            ErrorKind::NotFound => "NOT_FOUND",
            ErrorKind::RateLimit => "RESOURCE_EXHAUSTED",
            ErrorKind::Unavailable => "UNAVAILABLE",
            ErrorKind::Timeout => "DEADLINE_EXCEEDED",
            ErrorKind::Upstream | ErrorKind::Internal => "INTERNAL",
        },
    }
}

/// Checks the shape of an error envelope and returns `(type, message)`.
fn read_envelope(
    protocol: Protocol,
    body: &Value,
    status: u16,
) -> Result<(String, String), String> {
    let text = |value: &Value, what: &str| {
        value
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("`{what}` is not a string in {body}"))
    };
    match protocol {
        // `{"error":{"message","type","param","code"}}`, all four keys
        // present (`param` and `code` may be null).
        Protocol::OpenaiChat | Protocol::OpenaiResponses => {
            let error = body["error"]
                .as_object()
                .ok_or_else(|| format!("no `error` object in {body}"))?;
            for key in ["message", "type", "param", "code"] {
                if !error.contains_key(key) {
                    return Err(format!("`error.{key}` is missing in {body}"));
                }
            }
            for key in ["param", "code"] {
                if !(error[key].is_null() || error[key].is_string()) {
                    return Err(format!("`error.{key}` must be a string or null in {body}"));
                }
            }
            Ok((
                text(&error["type"], "error.type")?,
                text(&error["message"], "error.message")?,
            ))
        }
        // `{"type":"error","error":{"type","message"}}`.
        Protocol::Anthropic => {
            if body["type"] != "error" {
                return Err(format!("top-level `type` must be `error` in {body}"));
            }
            Ok((
                text(&body["error"]["type"], "error.type")?,
                text(&body["error"]["message"], "error.message")?,
            ))
        }
        // `{"error":{"code":<http status>,"message","status"}}`.
        Protocol::Gemini => {
            if body["error"]["code"] != status {
                return Err(format!(
                    "`error.code` must be the HTTP status {status} in {body}"
                ));
            }
            Ok((
                text(&body["error"]["status"], "error.status")?,
                text(&body["error"]["message"], "error.message")?,
            ))
        }
    }
}

fn every_kind(protocol: Protocol) {
    let mut failures = Failures::default();
    for kind in KINDS {
        let context = format!("{kind:?}");
        let message = format!("The {kind:?} thing went wrong for model `gpt-x`.");
        let error = ApiError::new(kind, message.clone());
        let body = codec(protocol).encode_error(&error);
        match read_envelope(protocol, &body, error.status) {
            Ok((kind_string, text)) => {
                let expected = expected_type(protocol, kind);
                failures.check(
                    &context,
                    kind_string == expected,
                    format!("type `{kind_string}`, expected `{expected}`"),
                );
                failures.check(
                    &context,
                    text == message,
                    format!("message changed to {text:?}"),
                );
            }
            Err(why) => failures.push(&context, why),
        }
        // The envelope reads back through the same protocol's decoder.
        let bytes = serde_json::to_vec(&body).expect("serialisable");
        let info = codec(protocol).decode_error(error.status, &bytes);
        failures.check(
            &context,
            info.message == message,
            format!("decode_error message {:?}", info.message),
        );
        failures.check(
            &context,
            info.error_type.as_deref() == Some(expected_type(protocol, kind)),
            format!("decode_error type {:?}", info.error_type),
        );
    }

    // Code, parameter and retry hint survive where the vendor has a slot.
    let error =
        ApiError::unknown_model("gpt-nope").with_retry_after(std::time::Duration::from_secs(7));
    let body = codec(protocol).encode_error(&error);
    let info = codec(protocol).decode_error(
        error.status,
        &serde_json::to_vec(&body).expect("serialisable"),
    );
    failures.check(
        "unknown_model",
        info.message.contains("gpt-nope"),
        format!("message {:?}", info.message),
    );
    match protocol {
        Protocol::OpenaiChat | Protocol::OpenaiResponses => {
            failures.check(
                "unknown_model",
                body["error"]["code"] == "model_not_found",
                format!("code {}", body["error"]["code"]),
            );
            failures.check(
                "unknown_model",
                body["error"]["param"] == "model",
                format!("param {}", body["error"]["param"]),
            );
            failures.check(
                "unknown_model",
                info.code.as_deref() == Some("model_not_found"),
                format!("decoded code {:?}", info.code),
            );
        }
        Protocol::Gemini => {
            // The retry hint travels in the body (notes 15 §6.6).
            failures.check(
                "unknown_model",
                info.retry_after_ms == Some(7000),
                format!("retry hint {:?}", info.retry_after_ms),
            );
        }
        Protocol::Anthropic => {}
    }
    failures.finish(&format!("{} errors", short(protocol)));
}

#[test]
fn chat_renders_every_error_kind() {
    every_kind(CHAT);
}

#[test]
fn responses_renders_every_error_kind() {
    every_kind(RESPONSES);
}

#[test]
fn anthropic_renders_every_error_kind() {
    every_kind(ANTHROPIC);
}

#[test]
fn gemini_renders_every_error_kind() {
    every_kind(GEMINI);
}

/// A vendor error body with what its decoder must find in it.
struct VendorError {
    name: &'static str,
    status: u16,
    body: String,
    message_contains: &'static str,
    error_type: Option<&'static str>,
    code: Option<&'static str>,
    retry_after_ms: Option<u64>,
}

fn vendor(
    name: &'static str,
    status: u16,
    body: Value,
    message_contains: &'static str,
) -> VendorError {
    VendorError {
        name,
        status,
        body: body.to_string(),
        message_contains,
        error_type: None,
        code: None,
        retry_after_ms: None,
    }
}

impl VendorError {
    fn kind(mut self, error_type: &'static str) -> Self {
        self.error_type = Some(error_type);
        self
    }

    fn code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }

    fn retry(mut self, ms: u64) -> Self {
        self.retry_after_ms = Some(ms);
        self
    }
}

/// Error bodies as the vendors document them (notes 15 §4.4, §5.5, §6.6),
/// plus the bodies proxies in front of them produce.
fn vendor_errors(upstream: Protocol) -> Vec<VendorError> {
    let mut errors = match upstream {
        Protocol::OpenaiChat | Protocol::OpenaiResponses => vec![
            vendor(
                "rate_limit",
                429,
                json!({"error": {
                    "message": "Rate limit reached for gpt-5.5 on tokens per min (TPM): Limit 30000, Used 29500, Requested 2000. Please try again in 3.2s.",
                    "type": "tokens", "param": null, "code": "rate_limit_exceeded"}}),
                "Rate limit reached",
            )
            .kind("tokens")
            .code("rate_limit_exceeded")
            .retry(3200),
            vendor(
                "quota",
                429,
                json!({"error": {
                    "message": "You exceeded your current quota, please check your plan and billing details.",
                    "type": "insufficient_quota", "param": null, "code": "insufficient_quota"}}),
                "exceeded your current quota",
            )
            .kind("insufficient_quota")
            .code("insufficient_quota"),
            vendor(
                "invalid_request",
                400,
                json!({"error": {
                    "message": "Invalid value for 'service_tier'.",
                    "type": "invalid_request_error", "param": "service_tier", "code": null}}),
                "service_tier",
            )
            .kind("invalid_request_error"),
            vendor(
                "overloaded",
                503,
                json!({"error": {
                    "message": "The server is overloaded. Please try again later.",
                    "type": "service_unavailable_error", "param": null, "code": "server_is_overloaded"}}),
                "overloaded",
            )
            .kind("service_unavailable_error")
            .code("server_is_overloaded"),
        ],
        Protocol::Anthropic => vec![
            vendor(
                "overloaded",
                529,
                json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}, "request_id": "req_011CSHoEeqs5C35K2UUqR7Fy"}),
                "Overloaded",
            )
            .kind("overloaded_error"),
            vendor(
                "rate_limit",
                429,
                json!({"type": "error", "error": {"type": "rate_limit_error",
                    "message": "This request would exceed your organization's rate limit of 30,000 input tokens per minute."}}),
                "rate limit",
            )
            .kind("rate_limit_error"),
            vendor(
                "invalid_request",
                400,
                json!({"type": "error", "error": {"type": "invalid_request_error",
                    "message": "messages.1.content.0: Invalid `signature` in `thinking` block"}}),
                "Invalid `signature`",
            )
            .kind("invalid_request_error"),
            vendor(
                "not_found",
                404,
                json!({"type": "error", "error": {"type": "not_found_error", "message": "model: claude-nope"}}),
                "claude-nope",
            )
            .kind("not_found_error"),
        ],
        Protocol::Gemini => vec![
            vendor(
                "rate_limit",
                429,
                json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED",
                    "message": "You exceeded your current quota. Please retry in 32.5s.",
                    "details": [
                        {"@type": "type.googleapis.com/google.rpc.QuotaFailure", "violations": [
                            {"quotaMetric": "generativelanguage.googleapis.com/generate_content_requests",
                             "quotaId": "GenerateRequestsPerMinutePerProjectPerModel"}]},
                        {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "32s"}
                    ]}}),
                "exceeded your current quota",
            )
            .kind("RESOURCE_EXHAUSTED")
            .retry(32_000),
            // An invalid key is a 400 with an `ErrorInfo` reason, not a 401.
            vendor(
                "invalid_key",
                400,
                json!({"error": {"code": 400, "status": "INVALID_ARGUMENT",
                    "message": "API key not valid. Please pass a valid API key.",
                    "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "API_KEY_INVALID",
                                 "domain": "googleapis.com", "metadata": {"service": "generativelanguage.googleapis.com"}}]}}),
                "API key not valid",
            )
            .kind("INVALID_ARGUMENT")
            .code("API_KEY_INVALID"),
            vendor(
                "overloaded",
                503,
                json!({"error": {"code": 503, "status": "UNAVAILABLE", "message": "The model is overloaded. Please try again later."}}),
                "overloaded",
            )
            .kind("UNAVAILABLE"),
            // Without RetryInfo the sentence at the end of the message is
            // the only hint.
            vendor(
                "retry_sentence",
                429,
                json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "message": "Resource exhausted. Please retry in 12.5s."}}),
                "Resource exhausted",
            )
            .kind("RESOURCE_EXHAUSTED")
            .retry(12_500),
        ],
    };
    // What sits between the gateway and any vendor.
    errors.push(VendorError {
        name: "html",
        status: 502,
        body: "<html><head><title>502 Bad Gateway</title></head><body><center><h1>502 Bad Gateway</h1></center></body></html>".to_string(),
        message_contains: "502",
        error_type: None,
        code: None,
        retry_after_ms: None,
    });
    errors.push(VendorError {
        name: "plain_text",
        status: 500,
        body: "upstream connect error or disconnect/reset before headers".to_string(),
        message_contains: "upstream connect error",
        error_type: None,
        code: None,
        retry_after_ms: None,
    });
    errors.push(VendorError {
        name: "empty",
        status: 503,
        body: String::new(),
        message_contains: "503",
        error_type: None,
        code: None,
        retry_after_ms: None,
    });
    errors
}

fn vendor_bodies(upstream: Protocol) {
    let mut failures = Failures::default();
    for case in vendor_errors(upstream) {
        let info = codec(upstream).decode_error(case.status, case.body.as_bytes());
        failures.check(
            case.name,
            info.message.contains(case.message_contains),
            format!(
                "message {:?} does not contain {:?}",
                info.message, case.message_contains
            ),
        );
        failures.check(case.name, !info.message.trim().is_empty(), "empty message");
        if let Some(expected) = case.error_type {
            failures.check(
                case.name,
                info.error_type.as_deref() == Some(expected),
                format!("type {:?}, expected {expected}", info.error_type),
            );
        }
        if let Some(expected) = case.code {
            failures.check(
                case.name,
                info.code.as_deref() == Some(expected),
                format!("code {:?}, expected {expected}", info.code),
            );
        }
        failures.check(
            case.name,
            info.retry_after_ms == case.retry_after_ms,
            format!(
                "retry hint {:?}, expected {:?}",
                info.retry_after_ms, case.retry_after_ms
            ),
        );
    }
    // Never fails, whatever the bytes.
    for garbage in [
        &b"\xff\xfe\x00\x01"[..],
        b"{",
        b"[]",
        b"null",
        b"\"text\"",
        b"{\"error\":null}",
    ] {
        let info = codec(upstream).decode_error(500, garbage);
        failures.check(
            "garbage",
            !info.message.is_empty(),
            format!("no message for {garbage:?}"),
        );
    }
    failures.finish(&format!("{} vendor errors", short(upstream)));
}

#[test]
fn chat_decodes_vendor_error_bodies() {
    vendor_bodies(CHAT);
}

#[test]
fn responses_decodes_vendor_error_bodies() {
    vendor_bodies(RESPONSES);
}

#[test]
fn anthropic_decodes_vendor_error_bodies() {
    vendor_bodies(ANTHROPIC);
}

#[test]
fn gemini_decodes_vendor_error_bodies() {
    vendor_bodies(GEMINI);
}

fn run_pair(client: Protocol, upstream: Protocol) {
    let mut failures = Failures::default();
    for case in vendor_errors(upstream) {
        let info = codec(upstream).decode_error(case.status, case.body.as_bytes());
        // What the gateway does with a failed attempt it cannot retry
        // (`docs/DESIGN.md` §8 step 6).
        let failure = UpstreamError {
            status: case.status,
            class: FailureClass::from_status(case.status),
            retry_after_ms: info.retry_after_ms,
            info,
            body: Some(case.body.clone()),
            content_type: None,
        };
        let error = failure.to_api_error();
        let body = codec(client).encode_error(&error);
        match read_envelope(client, &body, error.status) {
            Ok((kind_string, text)) => {
                let expected = expected_type(client, error.kind);
                failures.check(
                    case.name,
                    kind_string == expected,
                    format!("type `{kind_string}`, expected `{expected}`"),
                );
                failures.check(
                    case.name,
                    text.contains(case.message_contains),
                    format!("the upstream's message is lost: {text:?}"),
                );
            }
            Err(why) => failures.push(case.name, why),
        }
        // A rate-limited upstream tells the client when to come back.
        if let Some(ms) = case.retry_after_ms {
            failures.check(
                case.name,
                error.retry_after_secs == Some(ms.div_ceil(1000)),
                format!(
                    "retry_after_secs {:?} for a hint of {ms} ms",
                    error.retry_after_secs
                ),
            );
        }
    }
    failures.finish(&format!(
        "{} <- {} (errors)",
        short(client),
        short(upstream)
    ));
}

macro_rules! pairs {
    ($($name:ident: $client:expr => $upstream:expr;)*) => {
        $(
            #[test]
            fn $name() {
                run_pair($client, $upstream);
            }
        )*
    };
}

pairs! {
    chat_from_chat: CHAT => CHAT;
    chat_from_responses: CHAT => RESPONSES;
    chat_from_anthropic: CHAT => ANTHROPIC;
    chat_from_gemini: CHAT => GEMINI;
    responses_from_chat: RESPONSES => CHAT;
    responses_from_responses: RESPONSES => RESPONSES;
    responses_from_anthropic: RESPONSES => ANTHROPIC;
    responses_from_gemini: RESPONSES => GEMINI;
    anthropic_from_chat: ANTHROPIC => CHAT;
    anthropic_from_responses: ANTHROPIC => RESPONSES;
    anthropic_from_anthropic: ANTHROPIC => ANTHROPIC;
    anthropic_from_gemini: ANTHROPIC => GEMINI;
    gemini_from_chat: GEMINI => CHAT;
    gemini_from_responses: GEMINI => RESPONSES;
    gemini_from_anthropic: GEMINI => ANTHROPIC;
    gemini_from_gemini: GEMINI => GEMINI;
}
