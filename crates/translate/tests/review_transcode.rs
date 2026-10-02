//! Review findings for `transcode` (R-T1, R-T2, R-T3). Every test in this
//! file asserts the correct behaviour and failed against the implementation
//! as it was reviewed; the findings are fixed and the tests are kept as
//! regression tests. The doc comments describe the defect each one guards
//! against.

mod review_support;

use review_support::{CORRUPT, FakeCodec, FakeDecoder, canonical, finish_stop, start, wire};
use std::sync::Arc;
use switchyard_core::codec::{ClientCtx, Codec};
use switchyard_core::protocol::Protocol;
use switchyard_core::sse::SseEvent;
use switchyard_core::stream::StreamEvent;
use switchyard_translate::transcode::Transcoder;

fn translator() -> Transcoder {
    let client = FakeCodec::new(Protocol::Anthropic);
    Transcoder::translate(
        Box::new(FakeDecoder::default()),
        client.stream_encoder(&ClientCtx::new("alias")),
    )
}

fn forwarder(client_model: Option<&str>) -> Transcoder {
    Transcoder::passthrough(
        Box::new(FakeDecoder::default()),
        Arc::new(FakeCodec::new(Protocol::OpenaiChat)),
        client_model.map(str::to_string),
    )
}

/// R-T1. A decoder failure that happens AFTER the stream's terminal event
/// must not put an error event behind the protocol's final event.
///
/// `Transcoder::push` documents the translate-mode decode failure as
/// terminating the stream "exactly as `Transcoder::fail` would", and `fail`
/// documents (and implements) "If the stream already ended with a terminal
/// event, no error is added". The module docs say "A stream ends with exactly
/// one terminal canonical event (`Finish` or `Error`)". The decode-failure
/// path skips that check: it encodes `StreamEvent::Error` unconditionally, so
/// the client receives `Finish` followed by an error event, while
/// `error()` / `failed()` still claim the stream completed normally.
#[test]
fn decode_failure_after_the_terminal_event_adds_no_error_event() {
    let mut t = translator();
    let mut out = t.push(&wire(&start()));
    out.extend(t.push(&wire(&finish_stop())));
    assert!(t.is_terminal());

    // Trailing garbage after a complete response.
    let mut late = t.push(&SseEvent::data(CORRUPT));
    late.extend(t.finish());

    let errors: Vec<StreamEvent> = canonical(&late)
        .into_iter()
        .filter(|e| matches!(e, StreamEvent::Error(_)))
        .collect();
    assert_eq!(
        errors,
        Vec::new(),
        "an error event was sent to the client after the terminal Finish event"
    );
    // What the client was told and what the accessors report must agree.
    assert_eq!(t.error(), None);
    assert!(!t.failed());
    // The protocol terminator is still sent exactly once.
    assert_eq!(late.iter().filter(|e| e.is_done_marker()).count(), 1);
}

/// R-T2. In passthrough mode the side decoder must not be able to starve the
/// gateway's first-event gate.
///
/// The module docs tell the gateway to "hold back the output of
/// `Transcoder::push` until `Transcoder::saw_first_event` becomes true", and
/// promise that in passthrough "whatever [the decoder] makes of the stream
/// never changes what the client receives" (track requirement: "decoder errors
/// are swallowed — passthrough must still forward"). But `saw_first_event`
/// is only ever set from decoded canonical events, and after a decoder error
/// the decoder is never fed again, so the flag stays false for the rest of
/// the stream: a gateway following the documented protocol buffers the whole
/// response and then treats a delivered stream as "never started".
#[test]
fn passthrough_decoder_failure_does_not_starve_the_first_event_gate() {
    let mut t = forwarder(Some("alias"));

    let first = SseEvent::data(CORRUPT);
    assert_eq!(t.push(&first), vec![first.clone()]);
    assert!(t.decode_error().is_some());

    // The upstream keeps streaming and every event is forwarded …
    let second = wire(&StreamEvent::BlockStart {
        index: 0,
        block: switchyard_core::stream::BlockStart::Text,
    });
    assert_eq!(t.push(&second).len(), 1);
    let third = wire(&StreamEvent::TextDelta {
        index: 0,
        text: "hello".into(),
    });
    assert_eq!(t.push(&third).len(), 1);

    // … so the transcoder has handed upstream output to the caller and can
    // learn nothing more from the decoder: the gate must open.
    assert!(
        t.saw_first_event(),
        "three upstream events were forwarded but saw_first_event() is still false"
    );
}

/// R-T3. Rewriting the model name of a passthrough event must not change any
/// other value in that event.
///
/// DESIGN section 2: passthrough "Responses are forwarded as-is with the model
/// name rewritten". The rewrite goes `&str -> serde_json::Value -> String`,
/// and the workspace's `serde_json` is built without `float_roundtrip`, whose
/// default float parser is only accurate to about one unit in the last place.
/// Roughly one in eight 16/17-digit floats (Gemini `avgLogprobs`,
/// `logprobsResult`, any OpenAI-compatible server that prints doubles in
/// full) comes out as a *different number*. Every Chat chunk carries `model`
/// and every Gemini chunk carries `modelVersion`, so with an alias in use the
/// whole stream takes this path.
#[test]
fn passthrough_model_rewrite_leaves_other_numbers_untouched() {
    let mut t = forwarder(Some("alias"));
    for number in [
        "-1.6596847772598267",
        "-9.643518239999999",
        "-12.504198559999999",
        "-19.701910220000002",
    ] {
        let event = SseEvent::data(format!(
            r#"{{"model":"upstream-model","avgLogprobs":{number}}}"#
        ));
        let out = t.push(&event);
        assert_eq!(out.len(), 1);
        let prefix = r#"{"model":"alias","avgLogprobs":"#;
        let forwarded = out[0]
            .data
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix('}'))
            .unwrap_or_else(|| panic!("unexpected event shape: {}", out[0].data));
        // Compared as numbers (`f64::from_str` is correctly rounded), so a
        // fix is free to print the same value with different digits.
        let sent: f64 = number.parse().expect("test literal");
        let received: f64 = forwarded.parse().expect("forwarded number");
        assert_eq!(
            received.to_bits(),
            sent.to_bits(),
            "the upstream sent {number}, the client received {forwarded}"
        );
    }
}
