//! Property-style checks of the stream decoder against the non-streamed
//! decoder, over a few hundred generated responses.
//!
//! For every generated response the documented event stream is produced
//! (with this crate's encoder, whose output is asserted event by event in
//! `stream_encode.rs`) and then bent the ways real "compatible" upstreams
//! bend it: addresses missing, every item at output index 0, one item id for
//! everything, no `output_item.done`, an empty terminal `output`, nothing
//! but complete items, nothing but the terminal event. Whatever the shape,
//! the decoder must produce a valid sequence that accumulates to exactly
//! what the non-streamed decoder makes of the same response.
//!
//! Generation is driven by a fixed-seed generator: the test is deterministic.

use serde_json::{Value, json};
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::ir::{
    FinishReason, Part, Reasoning, RefusalPart, Response, Signature, ToolCall, ToolCallKind,
};
use switchyard_core::stream::{Accumulator, response_to_events, validate_sequence};
use switchyard_core::{ClientCtx, Codec, Protocol, SseEvent, Usage};

const P: Protocol = Protocol::OpenaiResponses;

/// xorshift64*: small, fast, reproducible.
struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as usize % bound
    }

    fn text(&mut self) -> String {
        const WORDS: &[&str] = &[
            "alpha",
            "β-beta",
            "{\"k\":1}",
            "line\nbreak",
            "naïve",
            "<tag>",
            "\"quoted\"",
            "末",
        ];
        let count = 1 + self.below(3);
        (0..count)
            .map(|_| WORDS[self.below(WORDS.len())])
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn generate(rng: &mut Rng, case: usize) -> Response {
    let mut response = Response::new(format!("resp_case{case}"), "upstream-model");
    response.created = 1_700_000_000 + case as i64;
    let mut calls = 0;
    for _ in 0..rng.below(7) {
        let part = match rng.below(6) {
            0 | 1 => Part::text(rng.text()),
            2 => Part::Refusal(RefusalPart { text: rng.text() }),
            3 => Part::Reasoning(Reasoning {
                id: None,
                text: if rng.below(4) == 0 {
                    String::new()
                } else {
                    rng.text()
                },
                // Always signed, so a text-less item still carries something.
                signature: Some(Signature::new(P, format!("gAAAA{}", rng.below(1000)))),
                redacted: false,
            }),
            kind => {
                calls += 1;
                let custom = kind == 5;
                Part::ToolCall(ToolCall {
                    id: format!("call_{case}_{calls}"),
                    name: ["lookup", "search", "exec"][rng.below(3)].to_string(),
                    arguments: match (custom, rng.below(3)) {
                        (true, _) => rng.text(),
                        (false, 0) => "{}".to_string(),
                        (false, _) => format!("{{\"q\":\"{}\",\"n\":{}}}", rng.below(50), calls),
                    },
                    kind: if custom {
                        ToolCallKind::Custom
                    } else {
                        ToolCallKind::Function
                    },
                    signature: None,
                    cache_control: None,
                })
            }
        };
        response.parts.push(part);
    }
    response.finish = match rng.below(5) {
        0 => FinishReason::Length,
        1 => FinishReason::ContentFilter,
        _ if calls > 0 => FinishReason::ToolCalls,
        _ => FinishReason::Stop,
    };
    response.usage = Usage {
        input_tokens: rng.below(5000) as u64,
        cache_read_tokens: rng.below(3) as u64 * 100,
        cache_write_tokens: 0,
        output_tokens: 50 + rng.below(500) as u64,
        reasoning_tokens: rng.below(50) as u64,
    };
    response
}

fn wire_events(response: &Response) -> Vec<Value> {
    let mut encoder = ResponsesCodec.stream_encoder(&ClientCtx::new("m"));
    let mut wire: Vec<SseEvent> = Vec::new();
    for event in response_to_events(response) {
        wire.extend(encoder.encode(&event));
    }
    wire.extend(encoder.finish());
    wire.iter()
        .map(|sse| serde_json::from_str(&sse.data).expect("JSON event"))
        .collect()
}

fn kind(event: &Value) -> &str {
    event["type"].as_str().unwrap_or("")
}

fn is_item_event(event: &Value) -> bool {
    matches!(
        kind(event),
        "response.output_item.added" | "response.output_item.done"
    )
}

fn is_terminal(event: &Value) -> bool {
    matches!(
        kind(event),
        "response.completed" | "response.incomplete" | "response.failed"
    )
}

fn remove(event: &mut Value, keys: &[&str]) {
    if let Some(map) = event.as_object_mut() {
        for key in keys {
            map.remove(*key);
        }
    }
}

fn each_final_item(event: &mut Value, mut change: impl FnMut(&mut Value)) {
    if let Some(items) = event["response"]["output"].as_array_mut() {
        items.iter_mut().for_each(&mut change);
    }
}

/// The ways an upstream may deviate from the documented stream.
const SHAPES: &[&str] = &[
    "as documented",
    "deltas without item id or output index",
    "no ids and no output indexes anywhere",
    "every item at output index 0",
    "one item id for every item",
    "one item id and output index 0 for every item",
    "no output_item.done",
    "empty terminal output",
    "deltas without addresses and an empty terminal output",
    "complete items only",
    "terminal event only",
    "no output_item.done and no addresses",
    "terminal output without ids",
    // Combinations (applied left to right).
    "one item id for every item + no output_item.done",
    "one item id for every item + complete items only",
    "one item id for every item + terminal event only",
    "every item at output index 0 + empty terminal output",
    "every item at output index 0 + terminal output without ids",
    "deltas without item id or output index + no output_item.done",
    "one item id and output index 0 for every item + empty terminal output",
];

fn reshape(shape: &str, events: &[Value]) -> Vec<Value> {
    if let Some((first, rest)) = shape.split_once(" + ") {
        return reshape(rest, &reshape(first, events));
    }
    let mut events = events.to_vec();
    match shape {
        "terminal output without ids" => {
            for event in &mut events {
                each_final_item(event, |item| remove(item, &["id"]));
            }
        }
        "as documented" => {}
        "deltas without item id or output index" => {
            for event in events.iter_mut().filter(|e| !is_item_event(e)) {
                remove(event, &["item_id", "output_index"]);
            }
        }
        "no ids and no output indexes anywhere" | "no output_item.done and no addresses" => {
            for event in &mut events {
                remove(event, &["item_id", "output_index"]);
                if is_item_event(event) {
                    remove(&mut event["item"], &["id"]);
                }
                each_final_item(event, |item| remove(item, &["id"]));
            }
            if shape == "no output_item.done and no addresses" {
                events.retain(|e| kind(e) != "response.output_item.done");
            }
        }
        "every item at output index 0" => {
            for event in &mut events {
                if event.get("output_index").is_some() {
                    event["output_index"] = json!(0);
                }
            }
        }
        "one item id for every item" | "one item id and output index 0 for every item" => {
            for event in &mut events {
                if event.get("item_id").is_some() {
                    event["item_id"] = json!("item_x");
                }
                if is_item_event(event) {
                    event["item"]["id"] = json!("item_x");
                }
                each_final_item(event, |item| item["id"] = json!("item_x"));
                if shape.contains("output index 0") && event.get("output_index").is_some() {
                    event["output_index"] = json!(0);
                }
            }
        }
        "no output_item.done" => events.retain(|e| kind(e) != "response.output_item.done"),
        "empty terminal output" => {
            for event in events.iter_mut().filter(|e| is_terminal(e)) {
                event["response"]["output"] = json!([]);
            }
        }
        "deltas without addresses and an empty terminal output" => {
            for event in &mut events {
                if !is_item_event(event) {
                    remove(event, &["item_id", "output_index"]);
                }
                if is_terminal(event) {
                    event["response"]["output"] = json!([]);
                }
            }
        }
        "complete items only" => {
            events.retain(|e| {
                kind(e) == "response.created"
                    || kind(e) == "response.output_item.done"
                    || is_terminal(e)
            });
            for event in events.iter_mut().filter(|e| is_terminal(e)) {
                event["response"]["output"] = json!([]);
            }
        }
        "terminal event only" => {
            events.retain(|e| kind(e) == "response.created" || is_terminal(e));
        }
        other => panic!("unknown shape {other}"),
    }
    events
}

/// What two decodings must agree on. Reasoning item ids are whatever the
/// upstream called the item, which the shapes above change on purpose.
fn essence(mut response: Response) -> (Vec<Part>, FinishReason, Usage) {
    for part in &mut response.parts {
        if let Part::Reasoning(reasoning) = part {
            reasoning.id = None;
        }
    }
    (response.parts, response.finish, response.usage)
}

fn decode_stream(events: &[Value], context: &str) -> Response {
    let mut decoder = ResponsesCodec.stream_decoder();
    let mut decoded = Vec::new();
    for event in events {
        let sse = SseEvent {
            event: event["type"].as_str().map(str::to_string),
            data: event.to_string(),
        };
        decoded.extend(decoder.decode(&sse).expect("decodable"));
    }
    decoded.extend(decoder.finish());
    if let Err(violation) = validate_sequence(&decoded) {
        panic!("{context}: {violation}\ninput: {events:#?}\noutput: {decoded:#?}");
    }
    let mut accumulator = Accumulator::new();
    for event in &decoded {
        accumulator.push(event);
    }
    accumulator.into_response()
}

#[test]
fn every_upstream_shape_decodes_to_what_the_non_streamed_decoder_sees() {
    let mut rng = Rng(0x5EED_CAFE_F00D_0001);
    let ctx = ClientCtx::new("m");
    for case in 0..300 {
        let original = generate(&mut rng, case);
        let body = ResponsesCodec
            .encode_response(&original, &ctx)
            .expect("encodes");
        let oracle = essence(ResponsesCodec.decode_response(&body).expect("decodes"));
        // The non-streamed round trip itself keeps the content.
        assert_eq!(oracle.0, original.parts, "case {case}");
        assert_eq!(oracle.2, original.usage, "case {case}");

        let events = wire_events(&original);
        for shape in SHAPES {
            let context = format!("case {case}, {shape}");
            let streamed = essence(decode_stream(&reshape(shape, &events), &context));
            assert_eq!(
                streamed, oracle,
                "{context}\nresponse: {original:#?}\nevents: {events:#?}"
            );
        }
    }
}
