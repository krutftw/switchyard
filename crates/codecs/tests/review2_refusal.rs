//! Review finding (round 2): an OpenAI model's refusal looks different to a
//! Chat client depending on which OpenAI protocol the upstream speaks. This
//! test currently FAILS.
//!
//! A refusal is a *completed* answer on both OpenAI protocols: Responses
//! reports `status: "completed"` with a `refusal` content part, Chat reports
//! `finish_reason: "stop"` with `message.refusal`. Notes 08 §8.2
//! (Responses -> Chat): "`response.completed` | `finish_reason:"tool_calls"`
//! if any tool call was emitted else `"stop"`"; `content_filter` is what
//! `response.incomplete` with reason `content_filter` maps to.
//!
//! Through the codecs a completed Responses refusal reaches a Chat client as
//! `finish_reason: "content_filter"`: the Responses decoder turns "completed,
//! refusal only" into `FinishReason::Refusal` and the Chat encoder renders
//! that as a filter stop. The same refusal from a Chat upstream decodes to
//! `Stop` and is rendered as `stop`. Clients tell the two apart: the OpenAI
//! SDK's structured-output helpers raise `ContentFilterFinishReasonError` on
//! `content_filter` instead of returning the refusal to the caller. Matrix
//! (b) expects `content_filter` here because its "refusal" fixtures were
//! written from the codec's table, not from the notes.

mod support;

use serde_json::{Value, json};
use support::harness::{
    CHAT, RESPONSES, client_ctx, decode_stream, encode_stream, over_the_wire, parse_sse,
};
use support::scenarios;
use switchyard_codecs::codec;

const REFUSAL: &str = "I can't help with that.";

fn responses_refusal() -> Value {
    json!({
        "id": "resp_ref1", "object": "response", "created_at": 1_759_400_000,
        "status": "completed", "incomplete_details": null, "error": null,
        "model": "gpt-5.5",
        "output": [{"type": "message", "id": "msg_ref1", "status": "completed", "role": "assistant",
                    "content": [{"type": "refusal", "refusal": REFUSAL}]}],
        "usage": {"input_tokens": 12, "output_tokens": 9, "total_tokens": 21}
    })
}

fn chat_refusal() -> Value {
    json!({
        "id": "chatcmpl-ref1", "object": "chat.completion", "created": 1_759_400_000,
        "model": "gpt-5.5",
        "choices": [{"index": 0, "finish_reason": "stop", "logprobs": null,
                     "message": {"role": "assistant", "content": null, "refusal": REFUSAL}}],
        "usage": {"prompt_tokens": 12, "completion_tokens": 9, "total_tokens": 21}
    })
}

#[test]
fn a_completed_refusal_is_a_stop_for_a_chat_client_whatever_the_openai_upstream() {
    let ctx = client_ctx(CHAT, &scenarios::tool_request(CHAT));
    let via = |upstream, body: &Value| {
        let decoded = codec(upstream)
            .decode_response(body)
            .expect("the refusal decodes");
        codec(CHAT)
            .encode_response(&decoded, &ctx)
            .expect("the refusal encodes")
    };
    let from_chat = via(CHAT, &chat_refusal());
    let from_responses = via(RESPONSES, &responses_refusal());
    for (label, seen) in [
        ("chat upstream", &from_chat),
        ("responses upstream", &from_responses),
    ] {
        assert_eq!(
            seen["choices"][0]["message"]["refusal"], REFUSAL,
            "{label}: the refusal text reaches the client"
        );
    }
    assert_eq!(from_chat["choices"][0]["finish_reason"], "stop");
    let complete = from_responses["choices"][0]["finish_reason"].clone();

    // The streamed form of the same answer.
    let transcript = format!(
        "event: response.created\ndata: {{\"type\":\"response.created\",\"sequence_number\":0,\"response\":{{\"id\":\"resp_ref1\",\"object\":\"response\",\"created_at\":1759400000,\"status\":\"in_progress\",\"model\":\"gpt-5.5\",\"output\":[]}}}}\n\n\
         event: response.output_item.added\ndata: {{\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{{\"id\":\"msg_ref1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}}}\n\n\
         event: response.content_part.added\ndata: {{\"type\":\"response.content_part.added\",\"sequence_number\":2,\"item_id\":\"msg_ref1\",\"output_index\":0,\"content_index\":0,\"part\":{{\"type\":\"refusal\",\"refusal\":\"\"}}}}\n\n\
         event: response.refusal.delta\ndata: {{\"type\":\"response.refusal.delta\",\"sequence_number\":3,\"item_id\":\"msg_ref1\",\"output_index\":0,\"content_index\":0,\"delta\":\"{REFUSAL}\"}}\n\n\
         event: response.refusal.done\ndata: {{\"type\":\"response.refusal.done\",\"sequence_number\":4,\"item_id\":\"msg_ref1\",\"output_index\":0,\"content_index\":0,\"refusal\":\"{REFUSAL}\"}}\n\n\
         event: response.completed\ndata: {{\"type\":\"response.completed\",\"sequence_number\":5,\"response\":{}}}\n\n",
        responses_refusal()
    );
    let events = decode_stream(RESPONSES, &parse_sse(&transcript));
    let wire = over_the_wire(&encode_stream(CHAT, &ctx, &events));
    let finish = wire
        .iter()
        .filter(|event| event.data != "[DONE]")
        .map(|event| serde_json::from_str::<Value>(&event.data).expect("json"))
        .find_map(|chunk| {
            chunk["choices"][0]["finish_reason"]
                .as_str()
                .map(str::to_string)
        })
        .expect("a finish chunk");
    let refused: String = wire
        .iter()
        .filter(|event| event.data != "[DONE]")
        .map(|event| serde_json::from_str::<Value>(&event.data).expect("json"))
        .filter_map(|chunk| {
            chunk["choices"][0]["delta"]["refusal"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    assert_eq!(refused, REFUSAL, "streamed: the refusal text arrives");

    assert_eq!(
        (complete, json!(finish)),
        (json!("stop"), json!("stop")),
        "(complete body, stream): notes 08 §8.2 — a `completed` response without tool calls \
         is `finish_reason: \"stop\"`"
    );
}
