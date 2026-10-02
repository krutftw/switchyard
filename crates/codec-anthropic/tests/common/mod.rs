//! Helpers shared by the integration tests.
#![allow(dead_code)]

use serde_json::Value;
use switchyard_codec_anthropic::AnthropicCodec;
use switchyard_core::ir::{Request, Response};
use switchyard_core::stream::{Accumulator, StreamEvent, validate_sequence};
use switchyard_core::{ClientCtx, Codec, RequestPath, SseEvent, SseParser, UpstreamCtx};

pub fn decode_request(body: &Value) -> Request {
    AnthropicCodec
        .decode_request(body, &RequestPath::default())
        .expect("request decodes")
}

pub fn encode_request(request: &Request) -> Value {
    AnthropicCodec
        .encode_request(request, &UpstreamCtx::default())
        .expect("request encodes")
}

pub fn encode_request_with(request: &Request, ctx: &UpstreamCtx<'_>) -> Value {
    AnthropicCodec
        .encode_request(request, ctx)
        .expect("request encodes")
}

pub fn decode_response(body: &Value) -> Response {
    AnthropicCodec
        .decode_response(body)
        .expect("response decodes")
}

pub fn encode_response(response: &Response, model: &str) -> Value {
    AnthropicCodec
        .encode_response(response, &ClientCtx::new(model))
        .expect("response encodes")
}

/// Splits raw SSE text into events with the gateway's own parser.
pub fn sse(wire: &str) -> Vec<SseEvent> {
    let mut parser = SseParser::new();
    let mut events = parser.push(wire.as_bytes()).expect("within size limit");
    events.extend(parser.finish());
    events
}

/// Runs a vendor transcript through the stream decoder, including the final
/// `finish()`, and checks the sequence contract.
pub fn decode_stream(wire: &str) -> Vec<StreamEvent> {
    decode_events(&sse(wire))
}

pub fn decode_events(events: &[SseEvent]) -> Vec<StreamEvent> {
    let mut decoder = AnthropicCodec.stream_decoder();
    let mut out = Vec::new();
    for event in events {
        out.extend(decoder.decode(event).expect("decoder never fails"));
    }
    out.extend(decoder.finish());
    // finish() must be idempotent.
    assert!(decoder.finish().is_empty());
    if let Err(violation) = validate_sequence(&out) {
        panic!("sequence contract violated: {violation}\n{out:#?}");
    }
    out
}

pub fn accumulate(events: &[StreamEvent]) -> Response {
    let mut accumulator = Accumulator::new();
    for event in events {
        accumulator.push(event);
    }
    accumulator.into_response()
}

/// Runs canonical events through the stream encoder, including `finish()`.
pub fn encode_stream(events: &[StreamEvent], model: &str) -> Vec<SseEvent> {
    let mut encoder = AnthropicCodec.stream_encoder(&ClientCtx::new(model));
    let mut out = Vec::new();
    for event in events {
        out.extend(encoder.encode(event));
    }
    out.extend(encoder.finish());
    assert!(encoder.finish().is_empty(), "finish() must be idempotent");
    out
}

/// Wire events as `(event name, parsed data)` pairs; also checks the
/// protocol rule that the event name equals the payload's `type`.
pub fn wire(events: &[SseEvent]) -> Vec<(String, Value)> {
    events
        .iter()
        .map(|event| {
            let name = event.event.clone().expect("every event is named");
            let data: Value = serde_json::from_str(&event.data).expect("data is JSON");
            assert_eq!(data["type"], Value::String(name.clone()));
            (name, data)
        })
        .collect()
}

pub fn names(events: &[SseEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| event.event.clone().unwrap_or_default())
        .collect()
}
