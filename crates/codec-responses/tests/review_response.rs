//! Regression test for a non-streamed response defect found in review (R3):
//! a failed upstream response decoded as a successful empty one. The
//! reviewer's original; the neighbouring cases are in `response.rs`.

use serde_json::json;
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::stream::StreamEvent;
use switchyard_core::{Codec, SseEvent};

/// A Responses upstream reports a generation failure with HTTP 200 and
/// `status: "failed"` + `error {code, message}` (15-api-research §4.2).
///
/// On the stream path the codec turns exactly this object into
/// `StreamEvent::Error` carrying the message and a derived status (the
/// gateway then fails over / cools the credential / tells the client why).
/// On the non-stream path `decode_response` returns `Ok` with an empty
/// response: the error text is dropped and the gateway sees a success. The
/// IR `Response` has no error slot, so the only way to keep the contract
/// ("decodes a complete upstream response", `CodecError::InvalidUpstream`
/// otherwise) is to return an error that carries the upstream's message.
#[test]
fn review_failed_response_is_reported_as_an_error_with_the_upstream_message() {
    let failed = json!({
        "id": "resp_1", "object": "response", "created_at": 1, "status": "failed", "model": "gpt-5",
        "output": [],
        "error": {"code": "rate_limit_exceeded", "message": "Rate limit reached for gpt-5. Please try again in 20s."},
        "incomplete_details": null,
        "usage": null
    });

    // Stream path, same object: an error with the message.
    let mut decoder = ResponsesCodec.stream_decoder();
    let events = decoder
        .decode(&SseEvent::named(
            "response.failed",
            json!({"type": "response.failed", "response": failed}).to_string(),
        ))
        .unwrap();
    let stream_error = events
        .iter()
        .find_map(|event| match event {
            StreamEvent::Error(error) => Some(error.clone()),
            _ => None,
        })
        .expect("stream path reports the failure");
    assert_eq!(stream_error.status, 429);
    assert!(stream_error.message.contains("Rate limit reached"));

    // Non-stream path must not turn it into a successful, empty response.
    match ResponsesCodec.decode_response(&failed) {
        Err(error) => assert!(
            error.to_string().contains("Rate limit reached"),
            "the error must carry the upstream message, got: {error}"
        ),
        Ok(response) => panic!(
            "a failed upstream response was decoded as a success and its error message was lost: {response:?}"
        ),
    }
}
