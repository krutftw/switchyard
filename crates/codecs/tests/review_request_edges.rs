//! Review findings on request translation edges the scenario library does
//! not cover. These tests currently FAIL.

mod support;

use serde_json::{Value, json};
use support::harness::{CHAT, GEMINI, RESPONSES, known_caps, translate_request, validate_request};
use switchyard_core::Protocol;

fn to(client: Protocol, upstream: Protocol, body: &Value) -> Value {
    let caps = known_caps(upstream);
    translate_request(client, upstream, body, &caps.ctx())
        .unwrap_or_else(|error| panic!("{client} -> {upstream} does not translate: {error}"))
}

/// A request that consists of instructions only is valid for both OpenAI
/// protocols (`messages: [{role: system}]`, or `instructions` with an empty
/// `input`). The Anthropic encoder gives such a request a placeholder user
/// turn; the Gemini encoder sends `contents: []`, which Gemini refuses
/// (400 "contents is not specified") and the matrix's own Gemini validator
/// rejects.
#[test]
fn a_system_only_request_gets_a_valid_gemini_body() {
    let cases = [
        (
            CHAT,
            json!({"model": "gpt-5.5", "messages": [{"role": "system", "content": "Write a haiku about rain."}]}),
        ),
        (
            RESPONSES,
            json!({"model": "gpt-5.5", "instructions": "Write a haiku about rain.", "input": []}),
        ),
    ];
    for (client, body) in &cases {
        let encoded = to(*client, GEMINI, body);
        assert!(
            encoded["systemInstruction"]
                .to_string()
                .contains("Write a haiku about rain."),
            "{client}: the instructions are missing: {encoded}"
        );
        if let Err(violations) = validate_request(GEMINI, &encoded) {
            panic!("{client}: Gemini would refuse this body: {violations:?}\n{encoded}");
        }
    }
}

/// Matrix (f) compares the depth fields only: `matrix_reasoning.rs` removes
/// `includeThoughts` (and `display`) before comparing, so the visibility half
/// of the notes-12 table is not checked. Notes 12 §7.4: "Because a Chat
/// `reasoning_effort` other than `none` counts as 'summaries on', Chat ->
/// Gemini requests end up with `includeThoughts: true`" (§8.1, the implicit
/// rule; §8.2 only suspends it for Claude targets). The Chat decoder reads no
/// summary intent from `reasoning_effort`, so a Chat client that turns
/// reasoning on gets no `reasoning_content` from a Gemini upstream at all:
/// Gemini's default is to return no thoughts.
#[test]
fn a_chat_reasoning_effort_asks_gemini_for_its_thoughts() {
    let body = json!({
        "model": "gpt-5.5",
        "messages": [{"role": "user", "content": "Prove that 91 is not prime."}],
        "reasoning_effort": "high"
    });
    let encoded = to(CHAT, GEMINI, &body);
    let config = &encoded["generationConfig"]["thinkingConfig"];
    assert_eq!(
        config["thinkingBudget"], 24576,
        "the depth itself is translated: {config}"
    );
    assert_eq!(
        config["includeThoughts"], true,
        "notes 12 §7.4 expects `includeThoughts: true` next to the depth: {config}"
    );
}

/// `ir::Request::candidate_count`: "Only `None`/`Some(1)` is supported across
/// protocols; other values survive same-family encoding." The gateway
/// renders exactly one candidate for the client (`decode_response` keeps
/// choice 0). The Gemini and Responses encoders leave a foreign count out;
/// the Chat encoder forwards a Gemini client's `candidateCount: 3` as
/// `n: 3`, so the upstream generates (and bills) three completions of which
/// two are thrown away.
#[test]
fn a_foreign_candidate_count_is_not_sent_to_a_chat_upstream() {
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "Name a colour."}]}],
        "generationConfig": {"candidateCount": 3}
    });
    let encoded = to(GEMINI, CHAT, &body);
    assert!(
        encoded.get("n").is_none() || encoded["n"] == 1,
        "the Chat upstream is asked for {} completions; the client can only be shown one: {encoded}",
        encoded["n"]
    );
}
