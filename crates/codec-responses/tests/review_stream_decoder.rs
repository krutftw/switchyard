//! Regression tests for stream decoder defects found in review: output
//! items sharing an item id (R1), events addressed to an item of another
//! kind (R5), and content applied twice from the terminal output array (R6),
//! plus the neighbouring cases found while fixing them. The `review_*` tests
//! are the reviewer's originals.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::ir::{Part, Response};
use switchyard_core::stream::{Accumulator, StreamEvent, validate_sequence};
use switchyard_core::{Codec, SseEvent};

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

fn accumulate(events: &[StreamEvent]) -> Response {
    let mut acc = Accumulator::new();
    for event in events {
        acc.push(event);
    }
    acc.into_response()
}

fn created() -> Value {
    json!({"type":"response.created","response":{"id":"resp_1","object":"response","created_at":1,"status":"in_progress","model":"m","output":[]}})
}

fn call_item(item_id: &str, call_id: &str, name: &str, arguments: &str, status: &str) -> Value {
    json!({"id": item_id, "type": "function_call", "status": status, "arguments": arguments, "call_id": call_id, "name": name})
}

/// Notes 08 §5.3 document a "compatible" upstream that reuses one item id
/// for several output items with different `output_index` values. The item
/// id must not be trusted over a *different* output index: two distinct
/// parallel tool calls must both survive, exactly as `decode_response` keeps
/// both when the same items arrive in a non-streamed body.
#[test]
fn review_tool_calls_sharing_an_item_id_are_both_kept() {
    let first = call_item(
        "fc_shared",
        "call_a",
        "lookup",
        "{\"q\":\"a\"}",
        "completed",
    );
    let second = call_item(
        "fc_shared",
        "call_b",
        "lookup",
        "{\"q\":\"b\"}",
        "completed",
    );
    let final_response = json!({
        "id": "resp_1", "object": "response", "created_at": 1, "status": "completed", "model": "m",
        "output": [first, second],
        "usage": {"input_tokens": 5, "output_tokens": 7, "total_tokens": 12}
    });

    let events = decode(&[
        created(),
        json!({"type":"response.output_item.added","output_index":0,"item":call_item("fc_shared","call_a","lookup","","in_progress")}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_shared","output_index":0,"delta":"{\"q\":\"a\"}"}),
        json!({"type":"response.output_item.done","output_index":0,"item":first}),
        json!({"type":"response.output_item.added","output_index":1,"item":call_item("fc_shared","call_b","lookup","","in_progress")}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_shared","output_index":1,"delta":"{\"q\":\"b\"}"}),
        json!({"type":"response.output_item.done","output_index":1,"item":second}),
        json!({"type":"response.completed","response":final_response}),
    ]);
    validate_sequence(&events).expect("valid sequence");
    let streamed = accumulate(&events);

    // What a non-streaming call decodes from the very same items.
    let non_streamed = ResponsesCodec.decode_response(&final_response).unwrap();
    assert_eq!(non_streamed.tool_calls().count(), 2);

    let calls: Vec<(&str, &str)> = streamed
        .tool_calls()
        .map(|c| (c.id.as_str(), c.arguments.as_str()))
        .collect();
    assert_eq!(
        calls,
        vec![("call_a", "{\"q\":\"a\"}"), ("call_b", "{\"q\":\"b\"}")],
        "the second tool call was merged into the first because both items carry the id `fc_shared`"
    );
}

/// The quirk exactly as the notes describe it (08 §5.3): a Chat-backed
/// Responses bridge names its reasoning item `rs_<resp>_<choice>`, so two
/// reasoning bursts in one response reuse the id with different output
/// indexes. The second burst must not vanish.
#[test]
fn review_reasoning_bursts_sharing_an_item_id_are_both_kept() {
    let reasoning = |text: &str| json!({"id":"rs_R_0","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":text}]});
    let events = decode(&[
        created(),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"rs_R_0","type":"reasoning","status":"in_progress","encrypted_content":"","summary":[]}}),
        json!({"type":"response.reasoning_summary_part.added","item_id":"rs_R_0","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_R_0","output_index":0,"summary_index":0,"delta":"first thought"}),
        json!({"type":"response.output_item.done","output_index":0,"item":reasoning("first thought")}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"msg_R_0","type":"message","status":"in_progress","content":[],"role":"assistant"}}),
        json!({"type":"response.content_part.added","item_id":"msg_R_0","output_index":1,"content_index":0,"part":{"type":"output_text","annotations":[],"text":""}}),
        json!({"type":"response.output_text.delta","item_id":"msg_R_0","output_index":1,"content_index":0,"delta":"Hello"}),
        json!({"type":"response.output_item.done","output_index":1,"item":{"id":"msg_R_0","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","annotations":[],"text":"Hello"}]}}),
        json!({"type":"response.output_item.added","output_index":2,"item":{"id":"rs_R_0","type":"reasoning","status":"in_progress","encrypted_content":"","summary":[]}}),
        json!({"type":"response.reasoning_summary_part.added","item_id":"rs_R_0","output_index":2,"summary_index":0,"part":{"type":"summary_text","text":""}}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_R_0","output_index":2,"summary_index":0,"delta":"second thought"}),
        json!({"type":"response.output_item.done","output_index":2,"item":reasoning("second thought")}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","model":"m","output":[]}}),
    ]);
    validate_sequence(&events).expect("valid sequence");
    let response = accumulate(&events);
    assert_eq!(
        response.reasoning_text(),
        "first thoughtsecond thought",
        "the second reasoning burst (same item id, output_index 2) was dropped"
    );
    assert_eq!(response.parts.len(), 3);
    assert!(matches!(response.parts[2], Part::Reasoning(_)));
}

/// DESIGN §3: "Stream decoders uphold the sequence contract for any input".
/// A reasoning delta addressed to an item that is a tool call must not come
/// out as a `ReasoningDelta` inside the tool-call block.
#[test]
fn review_reasoning_delta_addressed_to_a_tool_item_keeps_the_contract() {
    let events = decode(&[
        created(),
        json!({"type":"response.output_item.added","output_index":0,"item":call_item("fc_1","call_1","f","","in_progress")}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"fc_1","output_index":0,"summary_index":0,"delta":"hmm"}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"{}"}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","model":"m","output":[]}}),
    ]);
    if let Err(violation) = validate_sequence(&events) {
        panic!("sequence contract violated: {violation}\n{events:#?}");
    }
}

/// Same rule, the other way round: tool-argument deltas addressed to a
/// reasoning item must not be emitted as `ToolArgsDelta` in a reasoning block.
#[test]
fn review_argument_delta_addressed_to_a_reasoning_item_keeps_the_contract() {
    let events = decode(&[
        created(),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":0,"summary_index":0,"delta":"thinking"}),
        json!({"type":"response.function_call_arguments.delta","item_id":"rs_1","output_index":0,"delta":"{\"a\":1}"}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","model":"m","output":[]}}),
    ]);
    if let Err(violation) = validate_sequence(&events) {
        panic!("sequence contract violated: {violation}\n{events:#?}");
    }
}

/// An annotation event that points at a refusal part must not produce a
/// `Citation` inside a refusal block (`validate_sequence` only allows
/// citations in text blocks).
#[test]
fn review_annotation_on_a_refusal_part_keeps_the_contract() {
    let events = decode(&[
        created(),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_1","type":"message","status":"in_progress","content":[],"role":"assistant"}}),
        json!({"type":"response.content_part.added","item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"refusal","refusal":""}}),
        json!({"type":"response.refusal.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"I cannot"}),
        json!({"type":"response.output_text.annotation.added","item_id":"msg_1","output_index":0,"content_index":0,"annotation_index":0,
               "annotation":{"type":"url_citation","url":"https://example.com","title":"t","start_index":0,"end_index":1}}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","model":"m","output":[]}}),
    ]);
    if let Err(violation) = validate_sequence(&events) {
        panic!("sequence contract violated: {violation}\n{events:#?}");
    }
}

/// A minimal compatible server streams text deltas without `item_id` /
/// `output_index` and then sends the complete output in `response.completed`.
/// The module docs promise every piece of content is taken "from the first
/// place it shows up … and never twice"; here the text arrives twice.
#[test]
fn review_terminal_output_does_not_repeat_text_streamed_without_ids_or_indexes() {
    let events = decode(&[
        created(),
        json!({"type":"response.output_text.delta","delta":"Hel"}),
        json!({"type":"response.output_text.delta","delta":"lo"}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","model":"m","output":[
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello"}]}
        ],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}),
    ]);
    validate_sequence(&events).expect("valid sequence");
    assert_eq!(accumulate(&events).text(), "Hello");
}

fn message_item(id: Option<&str>, text: &str) -> Value {
    let mut item = json!({"type": "message", "status": "completed", "role": "assistant",
                          "content": [{"type": "output_text", "annotations": [], "text": text}]});
    if let Some(id) = id {
        item["id"] = json!(id);
    }
    item
}

fn completed(output: Value) -> Value {
    json!({"type": "response.completed", "response": {
        "id": "resp_1", "status": "completed", "model": "m", "output": output,
        "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}
    }})
}

fn decoded(events: &[Value]) -> Response {
    let events = decode(events);
    if let Err(violation) = validate_sequence(&events) {
        panic!("sequence contract violated: {violation}\n{events:#?}");
    }
    accumulate(&events)
}

/// The same duplication as above, one event earlier: bare deltas followed by
/// an `output_item.done` that does carry an address. The complete item must
/// be recognised as the one the deltas were feeding, and the terminal array
/// as that item again.
#[test]
fn item_done_does_not_repeat_text_streamed_without_ids_or_indexes() {
    for (done_id, final_id) in [
        (Some("msg_1"), Some("msg_1")),
        (Some("msg_1"), None),
        (None, Some("msg_1")),
        (None, None),
    ] {
        let response = decoded(&[
            created(),
            json!({"type":"response.output_text.delta","delta":"Hel"}),
            json!({"type":"response.output_text.delta","delta":"lo"}),
            json!({"type":"response.output_item.done","output_index":0,"item":message_item(done_id, "Hello")}),
            completed(json!([message_item(final_id, "Hello")])),
        ]);
        assert_eq!(
            response.parts,
            vec![Part::text("Hello")],
            "{done_id:?} / {final_id:?}"
        );
    }
}

/// Bare argument deltas arrive before anything names the call; the item that
/// completes it carries the address, the name and the same arguments.
#[test]
fn item_done_does_not_repeat_arguments_streamed_without_ids_or_indexes() {
    let call = call_item("fc_1", "call_1", "lookup", "{\"q\":1}", "completed");
    let response = decoded(&[
        created(),
        json!({"type":"response.function_call_arguments.delta","delta":"{\"q\":"}),
        json!({"type":"response.function_call_arguments.delta","delta":"1}"}),
        json!({"type":"response.output_item.done","output_index":0,"item":call}),
        completed(json!([call])),
    ]);
    assert_eq!(
        response.parts,
        vec![Part::tool_call("call_1", "lookup", "{\"q\":1}")]
    );
}

/// The terminal array may be shorter than what was streamed (reasoning left
/// out) or carry no ids, so positions and addresses do not line up. Streamed
/// content must still be recognised instead of being applied again.
#[test]
fn terminal_output_is_matched_when_positions_and_ids_do_not_line_up() {
    let stream = |final_output: Value| {
        decoded(&[
            created(),
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}),
            json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":0,"summary_index":0,"delta":"think"}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[]}}),
            json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"content_index":0,"delta":"Hello"}),
            completed(final_output),
        ])
    };
    let expected = vec![Part::reasoning("think"), Part::text("Hello")];
    let reasoning =
        json!({"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"think"}]});
    for final_output in [
        json!([reasoning, message_item(Some("msg_1"), "Hello")]),
        // the reasoning item is left out of the final array
        json!([message_item(Some("msg_1"), "Hello")]),
        // ... and the message has no id there
        json!([message_item(None, "Hello")]),
        // no ids at all
        json!([{"type":"reasoning","summary":[{"type":"summary_text","text":"think"}]}, message_item(None, "Hello")]),
        json!([]),
    ] {
        let response = stream(final_output.clone());
        let mut parts = response.parts.clone();
        for part in &mut parts {
            if let Part::Reasoning(reasoning) = part {
                reasoning.id = None;
            }
        }
        assert_eq!(parts, expected, "{final_output}");
    }
}

/// An item that only shows up in the terminal array is still delivered, next
/// to the ones that were streamed.
#[test]
fn terminal_output_adds_items_that_were_never_streamed() {
    let response = decoded(&[
        created(),
        json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"Calling."}),
        completed(json!([
            message_item(Some("msg_1"), "Calling."),
            call_item("fc_1", "call_1", "lookup", "{}", "completed"),
            call_item("fc_2", "call_2", "lookup", "{\"q\":2}", "completed")
        ])),
    ]);
    assert_eq!(
        response.parts,
        vec![
            Part::text("Calling."),
            Part::tool_call("call_1", "lookup", "{}"),
            Part::tool_call("call_2", "lookup", "{\"q\":2}"),
        ]
    );
}

/// R1 with the other reuse stacked on top: an upstream that gives every item
/// the same id *and* the same output index. Each announcement of a finished
/// item's address is a new item.
#[test]
fn items_sharing_both_id_and_output_index_are_kept_apart() {
    let first = call_item("fc_0", "call_a", "lookup", "{\"q\":\"a\"}", "completed");
    let second = call_item("fc_0", "call_b", "lookup", "{\"q\":\"b\"}", "completed");
    let expected = vec![
        Part::tool_call("call_a", "lookup", "{\"q\":\"a\"}"),
        Part::tool_call("call_b", "lookup", "{\"q\":\"b\"}"),
    ];
    let response = decoded(&[
        created(),
        json!({"type":"response.output_item.added","output_index":0,"item":call_item("fc_0","call_a","lookup","","in_progress")}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_0","output_index":0,"delta":"{\"q\":\"a\"}"}),
        json!({"type":"response.output_item.done","output_index":0,"item":first}),
        json!({"type":"response.output_item.added","output_index":0,"item":call_item("fc_0","call_b","lookup","","in_progress")}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_0","output_index":0,"delta":"{\"q\":\"b\"}"}),
        json!({"type":"response.output_item.done","output_index":0,"item":second}),
        completed(json!([first, second])),
    ]);
    assert_eq!(response.parts, expected);

    // Without `output_item.done` in between, the call ids tell them apart.
    let response = decoded(&[
        created(),
        json!({"type":"response.output_item.added","output_index":0,"item":call_item("fc_0","call_a","lookup","","in_progress")}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_0","output_index":0,"delta":"{\"q\":\"a\"}"}),
        json!({"type":"response.output_item.added","output_index":0,"item":call_item("fc_0","call_b","lookup","","in_progress")}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_0","output_index":0,"delta":"{\"q\":\"b\"}"}),
        completed(json!([first, second])),
    ]);
    assert_eq!(response.parts, expected);

    // Only complete items, no announcements.
    let response = decoded(&[
        created(),
        json!({"type":"response.output_item.done","output_index":0,"item":first}),
        json!({"type":"response.output_item.done","output_index":0,"item":second}),
        completed(json!([])),
    ]);
    assert_eq!(response.parts, expected);
}

/// A duplicated `output_item.done` (or the same item again in the terminal
/// array) is still one item.
#[test]
fn a_repeated_item_done_is_not_a_second_item() {
    let call = call_item("fc_1", "call_1", "lookup", "{}", "completed");
    let response = decoded(&[
        created(),
        json!({"type":"response.output_item.done","output_index":0,"item":call}),
        json!({"type":"response.output_item.done","output_index":0,"item":call}),
        json!({"type":"response.output_item.done","output_index":1,"item":message_item(Some("msg_1"), "Done.")}),
        json!({"type":"response.output_item.done","item":message_item(Some("msg_1"), "Done.")}),
        completed(json!([call, message_item(Some("msg_1"), "Done.")])),
    ]);
    assert_eq!(
        response.parts,
        vec![
            Part::tool_call("call_1", "lookup", "{}"),
            Part::text("Done.")
        ]
    );
}

/// An upstream that puts every item at output index 0 and sends neither
/// `output_item.added` nor `output_item.done`: the item ids and the kind of
/// content tell where one item ends and the next begins.
#[test]
fn deltas_for_another_id_at_a_reused_index_start_a_new_item() {
    let response = decoded(&[
        created(),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":0,"summary_index":0,"delta":"think"}),
        json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"Hel"}),
        json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"lo"}),
        completed(json!([])),
    ]);
    assert_eq!(response.reasoning_text(), "think");
    assert_eq!(response.text(), "Hello");
    assert_eq!(response.parts.len(), 2);
}

/// R5: the stray events are dropped, the item's own content is not.
#[test]
fn events_for_another_kind_do_not_disturb_the_item_they_address() {
    let response = decoded(&[
        created(),
        json!({"type":"response.output_item.added","output_index":0,"item":call_item("fc_1","call_1","f","","in_progress")}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"fc_1","output_index":0,"summary_index":0,"delta":"hmm"}),
        json!({"type":"response.output_text.delta","item_id":"fc_1","output_index":0,"content_index":0,"delta":"text?"}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"{\"a\":1}"}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"rs_1","type":"reasoning","summary":[]}}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":1,"summary_index":0,"delta":"thinking"}),
        json!({"type":"response.function_call_arguments.delta","item_id":"rs_1","output_index":1,"delta":"{\"b\":2}"}),
        json!({"type":"response.output_text.annotation.added","item_id":"rs_1","output_index":1,"content_index":0,"annotation_index":0,
               "annotation":{"type":"url_citation","url":"https://example.com","title":"t","start_index":0,"end_index":1}}),
        completed(json!([])),
    ]);
    assert_eq!(
        response.parts,
        vec![
            Part::tool_call("call_1", "f", "{\"a\":1}"),
            Part::Reasoning(switchyard_core::ir::Reasoning {
                id: Some("rs_1".into()),
                text: "thinking".into(),
                signature: None,
                redacted: false,
            }),
        ]
    );
}

/// An item announced without a `type` is whatever the first event that feeds
/// it says it is.
#[test]
fn an_untyped_item_takes_the_kind_of_its_first_content() {
    let response = decoded(&[
        created(),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"it_1"}}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"it_1","output_index":0,"summary_index":0,"delta":"think"}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"it_2"}}),
        json!({"type":"response.output_text.delta","item_id":"it_2","output_index":1,"content_index":0,"delta":"Hello"}),
        completed(json!([])),
    ]);
    assert_eq!(response.reasoning_text(), "think");
    assert_eq!(response.text(), "Hello");
}

/// A deterministic pseudo-random walk over single- and double-point
/// mutations of a realistic transcript (ids and indexes swapped or removed,
/// item types changed, events dropped, duplicated or reordered). Whatever
/// comes out must satisfy the sequence contract; nothing may panic.
#[test]
fn mutated_transcripts_always_decode_to_a_valid_sequence() {
    let reasoning_done = json!({"id":"rs_1","type":"reasoning","encrypted_content":"gAAAA","summary":[{"type":"summary_text","text":"think"}]});
    let call_done = call_item("fc_1", "call_1", "lookup", "{\"q\":1}", "completed");
    let message_done = json!({"id":"msg_1","type":"message","status":"completed","role":"assistant","content":[
        {"type":"output_text","annotations":[{"type":"url_citation","url":"https://example.com","title":"t","start_index":0,"end_index":2}],"text":"Hello"},
        {"type":"refusal","refusal":"No."}
    ]});
    let transcript = vec![
        created(),
        json!({"type":"response.in_progress","response":{"id":"resp_1","model":"m"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}),
        json!({"type":"response.reasoning_summary_part.added","item_id":"rs_1","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":0,"summary_index":0,"delta":"think"}),
        json!({"type":"response.reasoning_summary_text.done","item_id":"rs_1","output_index":0,"summary_index":0,"text":"think"}),
        json!({"type":"response.output_item.done","output_index":0,"item":reasoning_done}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.content_part.added","item_id":"msg_1","output_index":1,"content_index":0,"part":{"type":"output_text","annotations":[],"text":""}}),
        json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"content_index":0,"delta":"Hel"}),
        json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"content_index":0,"delta":"lo"}),
        json!({"type":"response.output_text.annotation.added","item_id":"msg_1","output_index":1,"content_index":0,"annotation_index":0,
               "annotation":{"type":"url_citation","url":"https://example.com","title":"t","start_index":0,"end_index":2}}),
        json!({"type":"response.output_text.done","item_id":"msg_1","output_index":1,"content_index":0,"text":"Hello"}),
        json!({"type":"response.content_part.done","item_id":"msg_1","output_index":1,"content_index":0,"part":{"type":"output_text","annotations":[],"text":"Hello"}}),
        json!({"type":"response.content_part.added","item_id":"msg_1","output_index":1,"content_index":1,"part":{"type":"refusal","refusal":""}}),
        json!({"type":"response.refusal.delta","item_id":"msg_1","output_index":1,"content_index":1,"delta":"No."}),
        json!({"type":"response.refusal.done","item_id":"msg_1","output_index":1,"content_index":1,"refusal":"No."}),
        json!({"type":"response.output_item.done","output_index":1,"item":message_done}),
        json!({"type":"response.output_item.added","output_index":2,"item":call_item("fc_1","call_1","lookup","","in_progress")}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":2,"delta":"{\"q\":"}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":2,"delta":"1}"}),
        json!({"type":"response.function_call_arguments.done","item_id":"fc_1","output_index":2,"arguments":"{\"q\":1}"}),
        json!({"type":"response.output_item.done","output_index":2,"item":call_done}),
        json!({"type":"response.output_item.done","output_index":3,"item":{"id":"ws_1","type":"web_search_call","status":"completed"}}),
        completed(
            json!([reasoning_done, message_done, call_done, {"id":"ws_1","type":"web_search_call","status":"completed"}]),
        ),
    ];

    // The unmutated transcript decodes to the obvious response.
    let clean = decoded(&transcript);
    assert_eq!(clean.parts.len(), 5, "{:#?}", clean.parts);
    assert_eq!(clean.text(), "Hello");

    let ids = ["rs_1", "msg_1", "fc_1", "ws_1", "", "other"];
    let types = [
        "reasoning",
        "message",
        "function_call",
        "custom_tool_call",
        "web_search_call",
        "",
    ];
    let kinds: Vec<String> = transcript
        .iter()
        .map(|event| event["type"].as_str().unwrap_or("").to_string())
        .collect();
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move |bound: usize| {
        // xorshift64*: plenty for picking mutations, and reproducible.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as usize % bound
    };
    for case in 0..6000 {
        let mut events = transcript.clone();
        for _ in 0..=next(2) {
            let at = next(events.len());
            match next(11) {
                0 => {
                    events.remove(at);
                }
                1 => {
                    let copy = events[at].clone();
                    events.insert(next(events.len() + 1), copy);
                }
                2 => {
                    let other = next(events.len());
                    events.swap(at, other);
                }
                3 => events[at]["item_id"] = json!(ids[next(ids.len())]),
                4 => events[at]["output_index"] = json!(next(4)),
                5 => {
                    if let Some(event) = events[at].as_object_mut() {
                        event.remove("item_id");
                        event.remove("output_index");
                    }
                }
                6 => {
                    if events[at].get("item").is_some() {
                        events[at]["item"]["type"] = json!(types[next(types.len())]);
                    }
                }
                7 => {
                    if events[at].get("item").is_some() {
                        events[at]["item"]["id"] = json!(ids[next(ids.len())]);
                    }
                }
                8 => events[at]["type"] = json!(kinds[next(kinds.len())]),
                9 => events[at]["content_index"] = json!(next(3)),
                _ => {
                    if events[at].get("part").is_some() {
                        let part_types =
                            ["output_text", "refusal", "reasoning_text", "summary_text"];
                        events[at]["part"]["type"] = json!(part_types[next(part_types.len())]);
                    }
                }
            }
        }
        let decoded = decode(&events);
        if let Err(violation) = validate_sequence(&decoded) {
            panic!("case {case}: {violation}\ninput: {events:#?}\noutput: {decoded:#?}");
        }
    }
}
