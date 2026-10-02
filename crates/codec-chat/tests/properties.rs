//! Randomised checks with a fixed seed (deterministic, offline):
//!
//! * whatever an upstream sends, the stream decoder produces a sequence that
//!   honours the canonical contract and never announces an unusable call;
//! * every response Chat can express survives `encode_response` ->
//!   `decode_response` and the stream encoder -> stream decoder round trip.

mod common;

use common::*;
use serde_json::{Value, json};
use switchyard_codec_chat::ChatCodec;
use switchyard_core::Usage;
use switchyard_core::codec::{ClientCtx, Codec};
use switchyard_core::ir::{
    FinishReason, Part, Reasoning, RefusalPart, Response, Signature, ToolCall, ToolCallKind,
};
use switchyard_core::protocol::Protocol;
use switchyard_core::sse::SseEvent;
use switchyard_core::stream::{BlockStart, StreamEvent, response_to_events};

/// xorshift64*: small, deterministic, good enough to explore state machines.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T: ?Sized>(&mut self, items: &[&'a T]) -> &'a T {
        items[self.below(items.len() as u64) as usize]
    }
}

// ---------------------------------------------------------------------------
// Arbitrary upstream streams
// ---------------------------------------------------------------------------

const FRAGMENTS: &[&str] = &[
    "",
    " ",
    "{",
    "}",
    "{}",
    "{\"a\":",
    "1}",
    "{\"a\":1}",
    "[1]",
    "\"x",
    "null",
];
const NAMES: &[&str] = &["alpha", "beta", ""];
const IDS: &[&str] = &["call_1", "call_2", ""];
const TEXTS: &[&str] = &["Hello", " world", "", "\n"];
const FINISHES: &[&str] = &[
    "stop",
    "length",
    "tool_calls",
    "content_filter",
    "function_call",
    "weird",
    "",
];

fn random_tool_delta(rng: &mut Rng) -> Value {
    let mut call = json!({});
    if rng.chance(85) {
        call["index"] = json!(rng.below(3));
    }
    if rng.chance(40) {
        call["id"] = json!(rng.pick(IDS));
    }
    if rng.chance(15) {
        call["type"] = json!(rng.pick(&["function", "custom"]));
    }
    if rng.chance(80) {
        let mut function = json!({});
        if rng.chance(45) {
            function["name"] = json!(rng.pick(NAMES));
        }
        if rng.chance(80) {
            function["arguments"] = json!(rng.pick(FRAGMENTS));
        }
        let holder = if rng.chance(10) { "custom" } else { "function" };
        if holder == "custom" && function.get("arguments").is_some() {
            function["input"] = function["arguments"].take();
        }
        call[holder] = function;
    }
    if rng.chance(5) {
        call["extra_content"] = json!({"google": {"thought_signature": "TS"}});
    }
    call
}

fn random_detail(rng: &mut Rng) -> Value {
    let mut detail = match rng.below(3) {
        0 => json!({"type": "reasoning.text", "text": rng.pick(TEXTS)}),
        1 => json!({"type": "reasoning.text", "signature": rng.pick(&["SIG_A", "SIG_B", ""])}),
        _ => json!({"type": "reasoning.encrypted", "data": rng.pick(&["ENC", ""])}),
    };
    if rng.chance(60) {
        detail["index"] = json!(rng.below(3));
    }
    if rng.chance(10) {
        detail["id"] = json!("rs_1");
    }
    detail
}

fn random_delta(rng: &mut Rng) -> Value {
    let mut delta = json!({});
    if rng.chance(15) {
        delta["role"] = json!("assistant");
    }
    if rng.chance(35) {
        delta["content"] = json!(rng.pick(TEXTS));
    }
    if rng.chance(15) {
        delta["reasoning_content"] = json!(rng.pick(TEXTS));
    }
    if rng.chance(8) {
        delta["reasoning"] = json!(rng.pick(TEXTS));
    }
    if rng.chance(15) {
        let n = 1 + rng.below(2);
        delta["reasoning_details"] = (0..n).map(|_| random_detail(rng)).collect();
    }
    if rng.chance(5) {
        delta["refusal"] = json!("no");
    }
    if rng.chance(40) {
        let n = 1 + rng.below(2);
        delta["tool_calls"] = (0..n).map(|_| random_tool_delta(rng)).collect();
    }
    if rng.chance(3) {
        delta["function_call"] = json!({"name": "legacy", "arguments": rng.pick(FRAGMENTS)});
    }
    if rng.chance(3) {
        delta["images"] =
            json!([{"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}]);
    }
    delta
}

fn random_event(rng: &mut Rng) -> SseEvent {
    let usage = json!({"prompt_tokens": rng.below(50), "completion_tokens": rng.below(50)});
    let data = match rng.below(100) {
        0..=2 => return SseEvent::data("[DONE]"),
        3 => return SseEvent::data("not json"),
        4 => json!({"error": {"message": "boom", "type": "server_error"}}),
        5..=8 => json!({"id": "chatcmpl-x", "model": "m", "choices": [], "usage": usage}),
        9 => json!({"id": "chatcmpl-x", "object": "ping"}),
        _ => {
            let mut choice = json!({"index": 0, "delta": random_delta(rng)});
            if rng.chance(8) {
                choice["finish_reason"] = json!(rng.pick(FINISHES));
            }
            let mut chunk = json!({"id": "chatcmpl-x", "model": "m", "choices": [choice]});
            if rng.chance(5) {
                chunk["usage"] = usage;
            }
            chunk
        }
    };
    SseEvent::data(data.to_string())
}

#[test]
fn any_chunk_sequence_decodes_to_a_valid_canonical_sequence() {
    let mut rng = Rng(0x5EED_CAFE_F00D_0001);
    for case in 0..6000 {
        let length = rng.below(14);
        let mut wire: Vec<SseEvent> = (0..length).map(|_| random_event(&mut rng)).collect();
        if rng.chance(50) {
            wire.push(SseEvent::data("[DONE]"));
        }
        // `decode_events` checks the sequence contract and `finish()`
        // idempotence.
        let events = decode_events(&wire);
        for event in &events {
            if let StreamEvent::BlockStart {
                block: BlockStart::ToolCall { id, name, .. },
                ..
            } = event
            {
                assert!(
                    !id.is_empty() && !name.is_empty(),
                    "case {case}: unusable call announced\n{wire:#?}"
                );
            }
        }
        let response = accumulate(&events);
        if response.finish == FinishReason::ToolCalls {
            assert!(
                response.tool_calls().next().is_some(),
                "case {case}: a tool turn without tool calls\n{wire:#?}"
            );
            for call in response.tool_calls() {
                let complete = call.arguments.is_empty()
                    || serde_json::from_str::<Value>(&call.arguments).is_ok();
                assert!(
                    call.kind == ToolCallKind::Custom || complete,
                    "case {case}: a tool turn with cut-off arguments {:?}\n{wire:#?}",
                    call.arguments
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Arbitrary expressible responses
// ---------------------------------------------------------------------------

fn chat_signature(rng: &mut Rng) -> Signature {
    Signature::new(
        Protocol::OpenaiChat,
        rng.pick(&["SIG_A", "SIG_B", "c2lnbmF0dXJl"]),
    )
}

/// A response in the order a Chat message has: reasoning, text, refusal,
/// tool calls. `streamed` allows the shapes only the stream can tell apart
/// (several unsigned reasoning blocks); without it the reasoning either is
/// one unsigned block or carries at least one blob, which is when the
/// non-streamed message spells the blocks out in `reasoning_details`.
fn random_response(rng: &mut Rng, streamed: bool) -> Response {
    let mut response = Response::new("chatcmpl-rt", "m");
    response.created = 1_700_000_000;
    let mut parts = Vec::new();

    let blocks = rng.below(4);
    let mut any_blob = false;
    for i in 0..blocks {
        let redacted = rng.chance(15);
        let force_blob = !streamed && blocks > 1 && i + 1 == blocks && !any_blob;
        let signed = redacted || force_blob || rng.chance(40);
        let text = if redacted || (signed && rng.chance(25)) {
            String::new()
        } else {
            rng.pick(&["plan", "think\nagain", "step 1"]).to_string()
        };
        any_blob |= signed;
        parts.push(Part::Reasoning(Reasoning {
            id: (signed && rng.chance(30)).then(|| format!("rs_{i}")),
            text,
            signature: signed.then(|| chat_signature(rng)),
            redacted,
        }));
    }
    if rng.chance(70) {
        parts.push(Part::text(rng.pick(&["Hello", "a\nb", "  spaced  "])));
    }
    if rng.chance(10) {
        parts.push(Part::Refusal(RefusalPart {
            text: "I cannot help with that.".into(),
        }));
    }
    let calls = if rng.chance(50) { rng.below(4) } else { 0 };
    for i in 0..calls {
        let custom = rng.chance(15);
        parts.push(Part::ToolCall(ToolCall {
            id: format!("call_{i}"),
            name: rng.pick(&["alpha", "beta"]).to_string(),
            arguments: if custom {
                rng.pick(&["ls -la", "{ not json"]).to_string()
            } else {
                rng.pick(&["{}", "{\"a\":1}", "{\"q\":\"x y\",\"n\":[1,2]}"])
                    .to_string()
            },
            kind: if custom {
                ToolCallKind::Custom
            } else {
                ToolCallKind::Function
            },
            signature: rng.chance(20).then(|| chat_signature(rng)),
            cache_control: None,
        }));
    }
    response.finish = match (calls > 0, rng.below(10)) {
        (_, 0) => FinishReason::Length,
        (_, 1) => FinishReason::ContentFilter,
        (true, _) => FinishReason::ToolCalls,
        (false, _) => FinishReason::Stop,
    };
    if rng.chance(70) {
        let output = 1 + rng.below(500);
        response.usage = Usage {
            input_tokens: rng.below(1000),
            cache_read_tokens: rng.below(3) * 64,
            cache_write_tokens: rng.below(2) * 32,
            output_tokens: output,
            reasoning_tokens: rng.below(output + 1),
        };
    }
    response.parts = parts;
    response
}

#[test]
fn expressible_responses_round_trip_without_loss() {
    let mut rng = Rng(0x5EED_CAFE_F00D_0002);
    for case in 0..3000 {
        let response = random_response(&mut rng, false);
        let body = ChatCodec
            .encode_response(&response, &ClientCtx::new("m"))
            .expect("response encodes");
        let back = ChatCodec.decode_response(&body).expect("response decodes");
        assert_eq!(back, response, "case {case}\n{body:#}");
    }
}

#[test]
fn expressible_responses_round_trip_through_the_stream() {
    let mut rng = Rng(0x5EED_CAFE_F00D_0003);
    for case in 0..3000 {
        let response = random_response(&mut rng, true);
        let wire = encode_stream(&response_to_events(&response), &ctx_with_usage("m"));
        let back = accumulate(&decode_events(&wire));
        assert_eq!(back, response, "case {case}\n{:#?}", payloads(&wire));
    }
}
