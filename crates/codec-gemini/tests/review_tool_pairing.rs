//! Review evidence: `functionResponse.name` must be the name of the call the
//! response is paired with.
//!
//! Gemini pairs a `functionResponse` with its `functionCall` by position and
//! **name**. `encode_request` resolves the name of a name-less tool result
//! with `Request::tool_name_for_call`, which returns the *first* call in the
//! whole conversation that has that id. Clients that reuse call ids from one
//! turn to the next (`call_1` every turn, or the empty id some
//! OpenAI-compatible servers hand out) therefore get responses named after a
//! tool from an earlier turn, although `pair_results` has matched them to the
//! calls of the turn right before.
//!
//! Source: notes 07 section 1.4 ("Tool-result lookup is scoped to the
//! assistant turn ... `name` comes from the call, not from the tool message
//! ... Scoping is what makes a client that reuses `call_1` in two different
//! turns work") and its checklist in section 10 ("repeated `tool_call_id`
//! across turns resolved per turn").

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_gemini::{GeminiCodec, SKIP_SIGNATURE};
use switchyard_core::ir::{Message, Part, Request, Role};
use switchyard_core::{Codec, Protocol, UpstreamCtx};

fn encode(messages: Vec<Message>) -> Value {
    let mut request = Request::new("gemini-2.5-pro", Protocol::OpenaiChat);
    request.messages = messages;
    GeminiCodec
        .encode_request(&request, &UpstreamCtx::default())
        .expect("request encodes")
}

#[test]
fn call_id_reused_in_a_later_turn_names_the_response_after_its_own_call() {
    let body = encode(vec![
        Message::user_text("go"),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call("call_1", "tool_a", "{}")],
        ),
        Message::new(Role::User, vec![Part::tool_result_text("call_1", "res_a")]),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call("call_1", "tool_b", "{}")],
        ),
        Message::new(Role::User, vec![Part::tool_result_text("call_1", "res_b")]),
    ]);
    assert_eq!(
        body["contents"],
        json!([
            {"role": "user", "parts": [{"text": "go"}]},
            {"role": "model", "parts": [
                {"functionCall": {"name": "tool_a", "args": {}}, "thoughtSignature": SKIP_SIGNATURE}]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "tool_a", "response": {"result": "res_a"}}}]},
            {"role": "model", "parts": [
                {"functionCall": {"name": "tool_b", "args": {}}, "thoughtSignature": SKIP_SIGNATURE}]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "tool_b", "response": {"result": "res_b"}}}]}
        ])
    );
}

#[test]
fn identical_call_ids_inside_one_turn_are_named_in_call_order() {
    // Some OpenAI-compatible servers give every tool call the same (empty)
    // id. The results are matched to the calls in order; each response must
    // carry the name of the call it answers.
    let body = encode(vec![
        Message::user_text("go"),
        Message::new(
            Role::Assistant,
            vec![
                Part::tool_call("", "alpha", "{}"),
                Part::tool_call("", "beta", "{}"),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                Part::tool_result_text("", "res_alpha"),
                Part::tool_result_text("", "res_beta"),
            ],
        ),
    ]);
    assert_eq!(
        body["contents"][2],
        json!({"role": "user", "parts": [
            {"functionResponse": {"name": "alpha", "response": {"result": "res_alpha"}}},
            {"functionResponse": {"name": "beta", "response": {"result": "res_beta"}}}
        ]})
    );
}
