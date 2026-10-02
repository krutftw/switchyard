//! Review findings on matrix (d), the two-turn reasoning tool loop.
//!
//! `matrix_round_trip.rs` only exercises the complete-response form of turn 1
//! and only the one fixture per upstream. These tests cover what it leaves
//! out and currently FAIL:
//!
//! 1. a **streaming Gemini client**. The Gemini stream encoder sends the
//!    signature of a reasoning block in a part of its own
//!    (`{"text":"","thought":true,"thoughtSignature":…}`) after the thought
//!    text. Gemini SDKs keep the parts of a stream as they arrived (python and
//!    JS `google-genai` chats: one model content per chunk; Gemini CLI: all
//!    parts in one content) and send them back. `decode_request` does not put
//!    the two halves together again, so the upstream that issued the blob
//!    gets it back detached from its text, or not at all;
//! 2. **redacted reasoning** through a Gemini client: the "redacted" marker
//!    has no carrier there, so Anthropic's `redacted_thinking.data` comes
//!    back as the signature of an empty `thinking` block;
//! 3. a **signature that sits on the tool call** (Gemini `thoughtSignature`
//!    on a `functionCall`, Google's Chat-compatible `extra_content`) through
//!    an Anthropic or a Responses client. The track requirement lists both as
//!    clients that have a slot for the upstream's blob; the codecs drop it
//!    and the upstream is sent the validator-bypass literal instead.

mod support;

use serde_json::{Value, json};
use support::harness::{
    ANTHROPIC, CHAT, GEMINI, RESPONSES, client_ctx, decode_stream, encode_stream, known_caps,
    over_the_wire, parse_sse, translate_request, validate_request,
};
use support::scenarios::{self, UpstreamResponse};
use switchyard_codecs::codec;
use switchyard_core::stream::StreamEvent;
use switchyard_core::{Protocol, SseEvent};

const TOOL_OUTPUT: &str = "15 degrees and cloudy";

fn fixture(upstream: Protocol, name: &str) -> UpstreamResponse {
    scenarios::responses(upstream)
        .into_iter()
        .find(|fixture| fixture.name == name)
        .unwrap_or_else(|| panic!("{upstream} has no `{name}` fixture"))
}

/// Turn 1 of a `client` that asks for reasoning and declares the tools the
/// fixtures call.
fn turn1(client: Protocol) -> Value {
    let mut body = scenarios::tool_request(client);
    let root = body.as_object_mut().expect("an object");
    match client {
        Protocol::OpenaiChat => {
            root.insert("reasoning_effort".into(), json!("high"));
        }
        Protocol::OpenaiResponses => {
            root.insert(
                "reasoning".into(),
                json!({"effort": "high", "summary": "auto"}),
            );
        }
        Protocol::Anthropic => {
            root.insert("max_tokens".into(), json!(16000));
            root.insert(
                "thinking".into(),
                json!({"type": "enabled", "budget_tokens": 4096}),
            );
        }
        Protocol::Gemini => {
            root.insert(
                "generationConfig".into(),
                json!({"thinkingConfig": {"thinkingBudget": 4096, "includeThoughts": true}}),
            );
        }
    }
    body
}

/// The upstream's streamed answer as canonical events.
fn upstream_events(upstream: Protocol, fixture: &UpstreamResponse) -> Vec<StreamEvent> {
    decode_stream(upstream, &parse_sse(fixture.sse.expect("a transcript")))
}

/// What a Gemini client is sent for a streamed answer: the JSON of every
/// chunk, after a trip over the wire.
fn gemini_chunks(request: &Value, events: &[StreamEvent]) -> Vec<Value> {
    let wire: Vec<SseEvent> =
        over_the_wire(&encode_stream(GEMINI, &client_ctx(GEMINI, request), events));
    wire.iter()
        .map(|event| serde_json::from_str::<Value>(&event.data).expect("a JSON chunk"))
        .collect()
}

/// How a Gemini SDK records a streamed model turn.
#[derive(Clone, Copy, Debug)]
enum History {
    /// `google-genai` (Python, JS) chats: the `content` of every chunk is
    /// appended to the history as a model content of its own.
    ContentPerChunk,
    /// Gemini CLI style: one model content holding the parts of all chunks,
    /// in order.
    OneContent,
}

/// Turn 2 of a Gemini client that streamed turn 1: the model contents as the
/// SDK recorded them, then one `functionResponse` per call.
fn gemini_turn2(chunks: &[Value], history: History) -> Value {
    let mut body = turn1(GEMINI);
    let streamed: Vec<Value> = chunks
        .iter()
        .filter_map(|chunk| chunk["candidates"][0].get("content").cloned())
        .collect();
    let mut responses = Vec::new();
    for content in &streamed {
        for part in content["parts"].as_array().into_iter().flatten() {
            if let Some(call) = part.get("functionCall") {
                let mut response =
                    json!({"name": call["name"], "response": {"output": TOOL_OUTPUT}});
                if let Some(id) = call.get("id") {
                    response["id"] = id.clone();
                }
                responses.push(json!({"functionResponse": response}));
            }
        }
    }
    assert!(!responses.is_empty(), "the streamed answer calls a tool");
    let contents = body["contents"].as_array_mut().expect("contents");
    match history {
        History::ContentPerChunk => contents.extend(streamed),
        History::OneContent => {
            let parts: Vec<Value> = streamed
                .iter()
                .flat_map(|content| content["parts"].as_array().cloned().unwrap_or_default())
                .collect();
            contents.push(json!({"role": "model", "parts": parts}));
        }
    }
    contents.push(json!({"role": "user", "parts": responses}));
    body
}

fn translate(client: Protocol, upstream: Protocol, body: &Value) -> Value {
    let caps = known_caps(upstream);
    let translated = translate_request(client, upstream, body, &caps.ctx())
        .unwrap_or_else(|error| panic!("turn 2 does not translate for {upstream}: {error}"));
    if let Err(violations) = validate_request(upstream, &translated) {
        panic!("turn 2 for {upstream} is not a valid request: {violations:?}\n{translated}");
    }
    translated
}

// ---------------------------------------------------------------------------
// 1. Streaming Gemini client
// ---------------------------------------------------------------------------

/// Anthropic upstream, Gemini client that streams. The signed thinking block
/// must come back as Anthropic sent it: same text, same signature, opening
/// the assistant turn (a `thinking` block whose text was changed is refused
/// by the API, and so is manual thinking without it).
fn gemini_stream_client_returns_anthropic_thinking(history: History) {
    let fixture = fixture(ANTHROPIC, "reasoning_tool");
    let blob = fixture.expect.blobs[0];
    let chunks = gemini_chunks(&turn1(GEMINI), &upstream_events(ANTHROPIC, &fixture));
    let turn2 = gemini_turn2(&chunks, history);
    let body = translate(GEMINI, ANTHROPIC, &turn2);

    let blocks = body["messages"][1]["content"]
        .as_array()
        .unwrap_or_else(|| panic!("no assistant content in {body}"));
    assert_eq!(
        blocks[0],
        json!({"type": "thinking", "thinking": fixture.expect.reasoning, "signature": blob}),
        "{history:?}: the thinking block Anthropic issued did not come back intact.\n\
         client turn 2: {}\nupstream body: {}",
        turn2["contents"],
        body["messages"]
    );
    assert_eq!(
        blocks
            .iter()
            .filter(|block| block["type"] == "thinking")
            .count(),
        1,
        "{history:?}: one thinking block was issued: {blocks:?}"
    );
    assert!(
        blocks.iter().any(|block| block["type"] == "tool_use"),
        "{history:?}: the tool call is gone: {blocks:?}"
    );
    assert_eq!(
        body["thinking"]["type"], "enabled",
        "{history:?}: thinking was switched off for the tool loop"
    );
}

#[test]
fn gemini_stream_client_content_per_chunk_returns_anthropic_thinking() {
    gemini_stream_client_returns_anthropic_thinking(History::ContentPerChunk);
}

#[test]
fn gemini_stream_client_one_content_returns_anthropic_thinking() {
    gemini_stream_client_returns_anthropic_thinking(History::OneContent);
}

/// Responses upstream, Gemini client that streams: the reasoning item with
/// its `encrypted_content` must be replayed directly ahead of the
/// function_call, with its summary text.
fn gemini_stream_client_returns_responses_reasoning(history: History) {
    let fixture = fixture(RESPONSES, "reasoning_tool");
    let blob = fixture.expect.blobs[0];
    let chunks = gemini_chunks(&turn1(GEMINI), &upstream_events(RESPONSES, &fixture));
    let turn2 = gemini_turn2(&chunks, history);
    let body = translate(GEMINI, RESPONSES, &turn2);

    let items = body["input"].as_array().expect("input");
    let at = items
        .iter()
        .position(|item| item["type"] == "reasoning" && item["encrypted_content"] == blob)
        .unwrap_or_else(|| {
            panic!(
                "{history:?}: the encrypted reasoning OpenAI issued was not replayed.\n\
                 client turn 2: {}\nupstream input: {}",
                turn2["contents"], body["input"]
            )
        });
    assert_eq!(
        items[at + 1]["type"],
        "function_call",
        "{history:?}: the reasoning item is not directly ahead of its function_call: {items:?}"
    );
    let summary: String = items[at]["summary"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect();
    assert_eq!(
        summary, fixture.expect.reasoning,
        "{history:?}: the reasoning item lost its summary"
    );
}

#[test]
fn gemini_stream_client_content_per_chunk_returns_responses_reasoning() {
    gemini_stream_client_returns_responses_reasoning(History::ContentPerChunk);
}

#[test]
fn gemini_stream_client_one_content_returns_responses_reasoning() {
    gemini_stream_client_returns_responses_reasoning(History::OneContent);
}

/// Chat upstream (a relay that signs its reasoning details), Gemini client
/// that streams: the reasoning detail must come back with text and signature
/// together, in the assistant message that makes the tool call.
#[test]
fn gemini_stream_client_returns_chat_reasoning_detail() {
    let fixture = fixture(CHAT, "reasoning_tool");
    let blob = fixture.expect.blobs[0];
    let chunks = gemini_chunks(&turn1(GEMINI), &upstream_events(CHAT, &fixture));
    let turn2 = gemini_turn2(&chunks, History::ContentPerChunk);
    let body = translate(GEMINI, CHAT, &turn2);

    let messages = body["messages"].as_array().expect("messages");
    let assistants: Vec<&Value> = messages
        .iter()
        .filter(|message| message["role"] == "assistant")
        .collect();
    assert_eq!(
        assistants.len(),
        1,
        "one model turn was streamed; it must be one assistant message, not one per chunk: {messages:?}"
    );
    let message = assistants[0];
    assert!(
        message["tool_calls"].is_array(),
        "the assistant message lost its tool call: {message}"
    );
    let details = message["reasoning_details"]
        .as_array()
        .unwrap_or_else(|| panic!("no reasoning_details in {message}"));
    assert!(
        details
            .iter()
            .any(|detail| detail["signature"] == blob
                && detail["text"] == fixture.expect.reasoning),
        "no reasoning detail carries the upstream's text together with its signature: {details:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Redacted reasoning through a Gemini client
// ---------------------------------------------------------------------------

/// Anthropic `redacted_thinking` must return as `redacted_thinking` with its
/// `data`. Chat and Responses clients have a carrier for the "redacted" flag
/// (`reasoning.encrypted`, the `redacted:` marker); for a Gemini client the
/// flag is lost and the payload is sent back as the `signature` of an empty
/// `thinking` block, which Anthropic answers with 400.
#[test]
fn gemini_client_returns_redacted_thinking_as_redacted_thinking() {
    let answer = json!({
        "id": "msg_01Red0000000000000000001", "type": "message", "role": "assistant",
        "model": "claude-sonnet-4-5",
        "content": [
            {"type": "redacted_thinking", "data": "EmwKRedactedPayload0123456789=="},
            {"type": "tool_use", "id": "toolu_01Red000000000000000001", "name": "get_weather",
             "input": {"location": "Paris"}}
        ],
        "stop_reason": "tool_use", "stop_sequence": null,
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let decoded = codec(ANTHROPIC)
        .decode_response(&answer)
        .expect("the answer decodes");

    for client in [CHAT, RESPONSES, GEMINI] {
        let request1 = turn1(client);
        let seen = codec(client)
            .encode_response(&decoded, &client_ctx(client, &request1))
            .expect("the answer encodes");
        let mut turn2 = request1.clone();
        match client {
            Protocol::OpenaiChat => {
                let message = seen["choices"][0]["message"].clone();
                let id = message["tool_calls"][0]["id"].clone();
                let messages = turn2["messages"].as_array_mut().expect("messages");
                messages.push(message);
                messages.push(json!({"role": "tool", "tool_call_id": id, "content": TOOL_OUTPUT}));
            }
            Protocol::OpenaiResponses => {
                let output = seen["output"].as_array().expect("output").clone();
                let input = turn2["input"].as_array_mut().expect("input");
                input.extend(output);
                input.push(json!({"type": "function_call_output",
                    "call_id": "toolu_01Red000000000000000001", "output": TOOL_OUTPUT}));
            }
            Protocol::Gemini => {
                let content = seen["candidates"][0]["content"].clone();
                let contents = turn2["contents"].as_array_mut().expect("contents");
                contents.push(content);
                contents.push(json!({"role": "user", "parts": [{"functionResponse": {
                    "name": "get_weather", "id": "toolu_01Red000000000000000001",
                    "response": {"output": TOOL_OUTPUT}}}]}));
            }
            Protocol::Anthropic => unreachable!(),
        }
        let body = translate(client, ANTHROPIC, &turn2);
        assert_eq!(
            body["messages"][1]["content"][0],
            json!({"type": "redacted_thinking", "data": "EmwKRedactedPayload0123456789=="}),
            "{client} client: the redacted_thinking block did not come back as Anthropic issued it.\n\
             the client was shown: {seen}\nupstream body: {}",
            body["messages"]
        );
    }
}

// ---------------------------------------------------------------------------
// 3. A signature on the tool call, through clients without a call slot
// ---------------------------------------------------------------------------

/// Turn 2 of a non-streaming Anthropic or Responses client.
fn echo_turn2(client: Protocol, answer: &Value) -> Value {
    let mut body = turn1(client);
    match client {
        Protocol::OpenaiResponses => {
            let output = answer["output"].as_array().expect("output").clone();
            let call_id = output
                .iter()
                .find(|item| item["type"] == "function_call")
                .map(|item| item["call_id"].clone())
                .expect("a function_call item");
            let input = body["input"].as_array_mut().expect("input");
            input.extend(output);
            input.push(
                json!({"type": "function_call_output", "call_id": call_id, "output": TOOL_OUTPUT}),
            );
        }
        Protocol::Anthropic => {
            let content = answer["content"].clone();
            let id = content
                .as_array()
                .and_then(|blocks| blocks.iter().find(|block| block["type"] == "tool_use"))
                .map(|block| block["id"].clone())
                .expect("a tool_use block");
            let messages = body["messages"].as_array_mut().expect("messages");
            messages.push(json!({"role": "assistant", "content": content}));
            messages.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "content": TOOL_OUTPUT}
            ]}));
        }
        other => panic!("not used for {other}"),
    }
    body
}

/// Track requirement 3d: "assert the U body carries U's signature blobs back
/// in their native positions when C has a slot for them (Anthropic,
/// Responses, Gemini clients; …)". Gemini's native position is the
/// `functionCall` part. An Anthropic client has `thinking.signature` and a
/// Responses client `reasoning.encrypted_content` to carry an opaque blob
/// (notes 09 §2.2: the reference hands a signed functionCall to a Claude
/// client as a thinking block ahead of the tool_use). The codecs drop the
/// blob instead and Gemini gets `skip_thought_signature_validator`, i.e. the
/// model's reasoning state is lost on every tool turn.
#[test]
fn gemini_call_signature_survives_anthropic_and_responses_clients() {
    let fixture = fixture(GEMINI, "reasoning_tool");
    let blob = fixture.expect.blobs[0];
    let decoded = codec(GEMINI)
        .decode_response(fixture.json.as_ref().expect("a body"))
        .expect("the answer decodes");
    let mut failures = Vec::new();
    for client in [ANTHROPIC, RESPONSES] {
        let request1 = turn1(client);
        let answer = codec(client)
            .encode_response(&decoded, &client_ctx(client, &request1))
            .expect("the answer encodes");
        let body = translate(client, GEMINI, &echo_turn2(client, &answer));
        let signature = body["contents"][1]["parts"]
            .as_array()
            .and_then(|parts| parts.iter().find(|part| part.get("functionCall").is_some()))
            .map(|part| part["thoughtSignature"].clone())
            .unwrap_or(Value::Null);
        if signature != blob {
            failures.push(format!(
                "{client} client: the replayed functionCall is signed with {signature}, \
                 not with the blob Gemini issued"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The same for Google's Chat-compatible endpoint, which signs the tool call
/// in `extra_content.google.thought_signature`.
#[test]
fn chat_call_signature_survives_anthropic_and_responses_clients() {
    let fixture = fixture(CHAT, "signed_tool_call");
    let blob = fixture.expect.blobs[0];
    let decoded = codec(CHAT)
        .decode_response(fixture.json.as_ref().expect("a body"))
        .expect("the answer decodes");
    let mut failures = Vec::new();
    for client in [ANTHROPIC, RESPONSES] {
        let request1 = turn1(client);
        let answer = codec(client)
            .encode_response(&decoded, &client_ctx(client, &request1))
            .expect("the answer encodes");
        let body = translate(client, CHAT, &echo_turn2(client, &answer));
        let signature =
            body["messages"][1]["tool_calls"][0]["extra_content"]["google"]["thought_signature"]
                .clone();
        if signature != blob {
            failures.push(format!(
                "{client} client: the replayed tool call is signed with {signature}, \
                 not with the blob the upstream issued"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
