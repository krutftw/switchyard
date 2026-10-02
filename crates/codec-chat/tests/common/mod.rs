//! Helpers shared by the integration tests.
#![allow(dead_code)]

use serde_json::{Value, json};
use std::sync::Arc;
use switchyard_codec_chat::ChatCodec;
use switchyard_core::codec::{ClientCtx, Codec, RequestPath, UpstreamCtx};
use switchyard_core::ir::{Request, Response};
use switchyard_core::sse::{SseEvent, SseParser};
use switchyard_core::stream::{Accumulator, StreamEvent, validate_sequence};

pub fn decode_request(body: Value) -> Request {
    ChatCodec
        .decode_request(&body, &RequestPath::default())
        .expect("request decodes")
}

pub fn encode_request(request: &Request) -> Value {
    ChatCodec
        .encode_request(request, &UpstreamCtx::default())
        .expect("request encodes")
}

/// The `messages` array of an encoded request.
pub fn encoded_messages(request: &Request) -> Value {
    encode_request(request)["messages"].clone()
}

/// Parses a raw SSE transcript the way the gateway does.
pub fn sse(transcript: &str) -> Vec<SseEvent> {
    let mut parser = SseParser::new();
    let mut events = parser.push(transcript.as_bytes()).expect("within limits");
    events.extend(parser.finish());
    events
}

/// Runs a transcript through a fresh decoder (including `finish()`) and
/// checks the canonical sequence contract.
pub fn decode_stream(transcript: &str) -> Vec<StreamEvent> {
    decode_events(&sse(transcript))
}

pub fn decode_events(events: &[SseEvent]) -> Vec<StreamEvent> {
    let mut decoder = ChatCodec.stream_decoder();
    let mut out = Vec::new();
    for event in events {
        out.extend(decoder.decode(event).expect("decoder never fails"));
    }
    out.extend(decoder.finish());
    assert!(decoder.finish().is_empty(), "finish() must be idempotent");
    if let Err(violation) = validate_sequence(&out) {
        panic!("sequence contract violated: {violation}\n{out:#?}");
    }
    out
}

pub fn accumulate(events: &[StreamEvent]) -> Response {
    let mut acc = Accumulator::new();
    for event in events {
        acc.push(event);
    }
    assert!(acc.started() && acc.finished());
    acc.into_response()
}

/// Runs canonical events through a fresh encoder (including `finish()`).
pub fn encode_stream(events: &[StreamEvent], ctx: &ClientCtx) -> Vec<SseEvent> {
    let mut encoder = ChatCodec.stream_encoder(ctx);
    let mut out = Vec::new();
    for event in events {
        out.extend(encoder.encode(event));
    }
    out.extend(encoder.finish());
    assert!(encoder.finish().is_empty(), "finish() must be idempotent");
    out
}

/// The JSON payload of every wire event; `[DONE]` becomes the string
/// `"[DONE]"`. Chat streams never use named events.
pub fn payloads(events: &[SseEvent]) -> Vec<Value> {
    events
        .iter()
        .map(|event| {
            assert_eq!(event.event, None, "chat chunks are data-only events");
            if event.is_done_marker() {
                json!("[DONE]")
            } else {
                serde_json::from_str(&event.data).expect("chunk is JSON")
            }
        })
        .collect()
}

/// A client context whose request asked for the streaming usage chunk.
pub fn ctx_with_usage(model: &str) -> ClientCtx {
    ClientCtx::new(model).with_request(Arc::new(json!({
        "model": model,
        "stream": true,
        "stream_options": {"include_usage": true}
    })))
}

/// The `choices[0].delta` of a chunk.
pub fn delta(chunk: &Value) -> &Value {
    &chunk["choices"][0]["delta"]
}
