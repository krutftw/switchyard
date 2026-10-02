//! Regression tests for an error handling defect found in review (R4):
//! upstream error text relayed without credential redaction. The `review_*`
//! tests are the reviewer's originals.

use serde_json::{Value, json};
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::stream::StreamEvent;
use switchyard_core::{ClientCtx, Codec, SseEvent};

const SECRET: &str = "sk-live-9f8e7d6c5b4a3f2e1d0c";

fn decode(events: &[Value]) -> Vec<StreamEvent> {
    let mut decoder = ResponsesCodec.stream_decoder();
    let mut out = Vec::new();
    for event in events {
        let name = event["type"].as_str().map(str::to_string);
        out.extend(
            decoder
                .decode(&SseEvent {
                    event: name,
                    data: event.to_string(),
                })
                .expect("decodable"),
        );
    }
    out.extend(decoder.finish());
    out
}

/// Notes 02 §9.5 / 03 §4.1 ("Sanitising"): all error text placed in a
/// Responses stream is redacted — `Bearer <token>` and values following
/// `api_key|access_token|token|authorization|secret` become `[REDACTED]`.
/// DESIGN "Rules for everyone": never log or return API keys.
///
/// An upstream (a proxy in front of the vendor, typically) that echoes the
/// request's credential in its in-stream error must not get that credential
/// relayed to the client through decoder + encoder.
#[test]
fn review_in_stream_error_text_is_redacted_before_it_reaches_the_client() {
    let upstream = [
        json!({"type":"response.created","response":{"id":"resp_1","model":"m","created_at":1}}),
        json!({"type":"error","code":"invalid_api_key","param":null,
               "message": format!("upstream rejected the request: Authorization: Bearer {SECRET} is not valid")}),
    ];
    let events = decode(&upstream);
    let mut encoder = ResponsesCodec.stream_encoder(&ClientCtx::new("m"));
    let mut wire = String::new();
    for event in &events {
        for sse in encoder.encode(event) {
            wire.push_str(&sse.data);
            wire.push('\n');
        }
    }
    for sse in encoder.finish() {
        wire.push_str(&sse.data);
    }
    assert!(
        wire.contains("\"type\":\"error\""),
        "terminal error event expected:\n{wire}"
    );
    assert!(
        !wire.contains(SECRET),
        "the upstream credential was relayed to the client verbatim:\n{wire}"
    );
}

/// Same rule for `response.failed` payloads.
#[test]
fn review_response_failed_error_text_is_redacted() {
    let events = decode(&[
        json!({"type":"response.failed","response":{"id":"resp_1","status":"failed",
               "error":{"code":"server_error","message": format!("backend call failed (api_key={SECRET})")}}}),
    ]);
    let leaked = events.iter().any(|event| match event {
        StreamEvent::Error(error) => error.message.contains(SECRET),
        _ => false,
    });
    assert!(
        !leaked,
        "StreamEvent::Error carries the upstream credential: {events:?}"
    );
}

/// The same sanitising applies to the pre-stream error path: what
/// `decode_error` extracts ends up in the client's error body and in the
/// request log.
#[test]
fn review_decode_error_redacts_credentials_echoed_by_the_upstream() {
    let json_body = json!({"error": {"type": "invalid_request_error", "code": "invalid_api_key",
        "message": format!("Invalid credentials in header Authorization: Bearer {SECRET}")}})
    .to_string();
    let info = ResponsesCodec.decode_error(401, json_body.as_bytes());
    assert!(
        !info.message.contains(SECRET),
        "JSON error body: credential kept in message: {}",
        info.message
    );

    let text_body = format!("401 Unauthorized: token={SECRET} was rejected by the origin");
    let info = ResponsesCodec.decode_error(401, text_body.as_bytes());
    assert!(
        !info.message.contains(SECRET),
        "plain-text error body: credential kept in message: {}",
        info.message
    );
}

/// What redaction leaves behind, exactly, on every path an upstream's error
/// text can take.
#[test]
fn redacted_error_text_keeps_everything_but_the_credential() {
    // HTTP error body, JSON.
    let body = json!({"error": {"type": "invalid_request_error", "code": "invalid_api_key",
        "message": format!("Invalid credentials in header Authorization: Bearer {SECRET}")}})
    .to_string();
    let info = ResponsesCodec.decode_error(401, body.as_bytes());
    assert_eq!(
        info.message,
        "Invalid credentials in header Authorization: Bearer [REDACTED]"
    );
    assert_eq!(info.code.as_deref(), Some("invalid_api_key"));
    assert_eq!(info.error_type.as_deref(), Some("invalid_request_error"));

    // HTTP error body, plain text and HTML.
    let info = ResponsesCodec.decode_error(
        401,
        format!("401 Unauthorized: token={SECRET} was rejected by the origin").as_bytes(),
    );
    assert_eq!(
        info.message,
        "401 Unauthorized: token=[REDACTED] was rejected by the origin"
    );
    let info = ResponsesCodec.decode_error(
        502,
        format!("<html><head><title>Bad key {SECRET}</title></head><body>x</body></html>")
            .as_bytes(),
    );
    assert_eq!(
        info.message,
        "upstream returned HTTP 502: Bad key [REDACTED]"
    );

    // A body with no message at all is summarised from its JSON, which may
    // hold secret-named keys.
    let info = ResponsesCodec.decode_error(
        500,
        br#"{"detail": {"upstream": "primary", "api_key": "abc123", "usage": {"total_tokens": 9}}}"#,
    );
    assert_eq!(
        info.message,
        r#"{"upstream":"primary","api_key":"[REDACTED]","usage":{"total_tokens":9}}"#
    );

    // In-stream error events, flat and nested, and `response.failed`.
    let stream_error = |event: Value| match decode(&[event]).pop() {
        Some(StreamEvent::Error(error)) => error,
        other => panic!("expected an error, got {other:?}"),
    };
    let flat = stream_error(
        json!({"type": "error", "code": "invalid_api_key", "param": null,
        "message": format!("upstream rejected the request: Authorization: Bearer {SECRET} is not valid")}),
    );
    assert_eq!(
        flat.message,
        "upstream rejected the request: Authorization: Bearer [REDACTED] is not valid"
    );
    assert_eq!(
        (flat.status, flat.code.as_deref()),
        (401, Some("invalid_api_key"))
    );
    let nested = stream_error(json!({"type": "error", "status": 502,
        "error": {"message": format!("proxy said: x-api-key: {SECRET}"), "type": "server_error"}}));
    assert_eq!(nested.message, "proxy said: x-api-key: [REDACTED]");
    let failed = stream_error(
        json!({"type": "response.failed", "response": {"id": "resp_1", "status": "failed",
        "error": {"code": "server_error", "message": format!("backend call failed (api_key={SECRET})")}}}),
    );
    assert_eq!(failed.message, "backend call failed (api_key=[REDACTED]");

    // Ordinary vendor messages come through untouched.
    let ordinary = "Rate limit reached for gpt-5 in organization org-abc on tokens per min (TPM): Limit 30000, Used 29000, Requested 2000. Please try again in 2s.";
    let event =
        stream_error(json!({"type": "error", "code": "rate_limit_exceeded", "message": ordinary}));
    assert_eq!(event.message, ordinary);
    assert_eq!(event.retry_after_secs, Some(2));
}

/// The client-facing side is the last line of defence: an error that some
/// other decoder took from an upstream may still carry a credential when it
/// reaches this codec's encoders.
#[test]
fn client_facing_errors_are_redacted_on_the_way_out() {
    let error = switchyard_core::ApiError::upstream(format!(
        "anthropic upstream said: invalid x-api-key: {SECRET}"
    ));
    let expected = "anthropic upstream said: invalid x-api-key: [REDACTED]";

    let body = ResponsesCodec.encode_error(&error);
    assert_eq!(body["error"]["message"], json!(expected));

    let mut encoder = ResponsesCodec.stream_encoder(&ClientCtx::new("m"));
    let wire = encoder.encode(&StreamEvent::Error(error.clone()));
    assert_eq!(wire.len(), 1);
    let event: Value = serde_json::from_str(&wire[0].data).unwrap();
    assert_eq!(event["type"], json!("error"));
    assert_eq!(event["message"], json!(expected));
    assert_eq!(event["error"]["message"], json!(expected));

    let frame = switchyard_codec_responses::ws_error_frame(502, &error.message, None, None);
    assert_eq!(frame["error"]["message"], json!(expected));
}

/// Statuses derived for in-stream failures that carry no status of their
/// own: the rest of the vendor-code table, and context overflows recognised
/// by their message (they are the request's fault whatever the code says, so
/// trying another credential is pointless).
#[test]
fn in_stream_failure_statuses_cover_the_code_table_and_context_overflows() {
    let status_of = |error: Value| match decode(&[json!({"type": "error", "error": error})]).pop() {
        Some(StreamEvent::Error(error)) => error.status,
        other => panic!("expected an error, got {other:?}"),
    };
    for (code, status) in [
        ("not_found_error", 404),
        ("not_found", 404),
        ("model_not_found", 404),
        ("unauthorized", 401),
        ("authentication_error", 401),
        ("forbidden", 403),
        ("permission_denied", 403),
        ("rate_limit_error", 429),
        ("invalid_request_error", 400),
        ("bad_request_error", 400),
        ("cyber_policy", 400),
        ("context_length_exceeded", 400),
        ("context_too_large", 400),
        ("something_new", 502),
    ] {
        assert_eq!(
            status_of(json!({"type": code, "message": "x"})),
            status,
            "type {code}"
        );
        assert_eq!(
            status_of(json!({"code": code, "message": "x"})),
            status,
            "code {code}"
        );
    }
    for message in [
        "Your input exceeds the context window of this model.",
        "This model's maximum context length is 128000 tokens.",
        "Too many tokens in the request.",
    ] {
        assert_eq!(
            status_of(json!({"code": "server_error", "message": message})),
            400,
            "{message}"
        );
    }
    // An explicit status still wins over both.
    assert_eq!(
        status_of(
            json!({"code": "rate_limit_exceeded", "status": 503, "message": "context window"})
        ),
        503
    );
}
