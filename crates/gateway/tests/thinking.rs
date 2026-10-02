//! Reasoning across protocols, over two turns: every client protocol
//! against every upstream protocol with reasoning switched on and a tool
//! call in the answer.
//!
//! Turn one: the upstream reasons — signed, the way its vendor signs — and
//! calls a tool. Turn two: the client sends the model turn back the way it
//! received it, plus the tool's result. The vendor that signed the
//! reasoning only accepts the conversation if the signature comes back
//! exactly as issued, in front of (or on) the tool call it led to. Real
//! signatures are long: thousands of characters, and through a Gemini
//! client they travel tagged and base64-armoured in `thoughtSignature`.

mod support;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{
    Answer, Behaviour, FOUR_PROVIDERS, Harness, Output, PROTOCOLS, body, codec, route,
    tool_declaration,
};
use switchyard_core::ir::{FinishReason, Part, Reasoning, Request, Response, Signature};
use switchyard_core::{ClientCtx, Protocol, RequestPath, UpstreamCtx};

const THOUGHT: &str = "Paris, then: the weather tool will know.";

/// A signature of `len` characters that looks like a vendor's: base64 text
/// over the whole alphabet, different at every position.
fn signature(len: usize) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut state = 0x2545_F491_u32;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ALPHABET[(state >> 26) as usize] as char
        })
        .collect()
}

/// A first turn in `client`'s protocol that offers the weather tool and
/// asks the model to reason.
fn turn_one(client: Protocol, model: &str, stream: bool) -> Value {
    let mut request = body(client, model, stream, true);
    match client {
        Protocol::OpenaiChat => request["reasoning_effort"] = json!("high"),
        Protocol::OpenaiResponses => {
            request["reasoning"] = json!({"effort": "high", "summary": "auto"});
            request["include"] = json!(["reasoning.encrypted_content"]);
            request["store"] = json!(false);
        }
        Protocol::Anthropic => {
            request["max_tokens"] = json!(8192);
            request["thinking"] = json!({"type": "enabled", "budget_tokens": 2048});
        }
        Protocol::Gemini => {
            request["generationConfig"] =
                json!({"thinkingConfig": {"thinkingBudget": 2048, "includeThoughts": true}});
        }
    }
    request
}

/// The second turn of a client that was answered with `answer` (a complete
/// response body in its protocol): the first turn, the model turn echoed
/// exactly as it arrived, and the tool's result.
fn turn_two(client: Protocol, first: &Value, answer: &Value) -> Value {
    let mut request = first.clone();
    match client {
        Protocol::OpenaiChat => {
            let message = answer["choices"][0]["message"].clone();
            let call_id = message["tool_calls"][0]["id"].clone();
            let messages = request["messages"].as_array_mut().unwrap();
            messages.push(message);
            messages
                .push(json!({"role": "tool", "tool_call_id": call_id, "content": "18 degrees"}));
        }
        Protocol::OpenaiResponses => {
            let items = answer["output"].as_array().cloned().unwrap();
            let call_id = items
                .iter()
                .find(|item| item["type"] == "function_call")
                .map(|item| item["call_id"].clone())
                .unwrap_or_else(|| panic!("no function call in {answer}"));
            let input = request["input"].as_array_mut().unwrap();
            input.extend(items);
            input.push(json!({
                "type": "function_call_output", "call_id": call_id, "output": "18 degrees"
            }));
        }
        Protocol::Anthropic => {
            let content = answer["content"].clone();
            let call_id = content
                .as_array()
                .and_then(|blocks| blocks.iter().find(|block| block["type"] == "tool_use"))
                .map(|block| block["id"].clone())
                .unwrap_or_else(|| panic!("no tool use in {answer}"));
            let messages = request["messages"].as_array_mut().unwrap();
            messages.push(json!({"role": "assistant", "content": content}));
            messages.push(json!({"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": call_id, "content": "18 degrees"
            }]}));
        }
        Protocol::Gemini => {
            let parts = answer["candidates"][0]["content"]["parts"].clone();
            push_gemini_turn(&mut request, parts);
        }
    }
    request
}

/// Appends the model turn made of `parts` and the tool's result to a Gemini
/// request.
fn push_gemini_turn(request: &mut Value, parts: Value) {
    let contents = request["contents"].as_array_mut().unwrap();
    contents.push(json!({"role": "model", "parts": parts}));
    contents.push(json!({"role": "user", "parts": [{"functionResponse": {
        "name": "get_weather", "response": {"temperature": "18 degrees"}
    }}]}));
}

/// The reasoning signature an upstream request carries for the model turn,
/// and whether it stands where the vendor wants it: in front of the tool
/// call (Anthropic, Responses) or on it (Gemini).
fn replayed_signature(upstream: Protocol, sent: &Value) -> Option<(String, bool)> {
    match upstream {
        Protocol::Anthropic => {
            let blocks = sent["messages"]
                .as_array()?
                .iter()
                .find(|message| message["role"] == "assistant")?["content"]
                .as_array()?;
            let thinking = blocks
                .iter()
                .position(|block| block["type"] == "thinking")?;
            let tool_use = blocks
                .iter()
                .position(|block| block["type"] == "tool_use")?;
            assert_eq!(
                blocks[thinking]["thinking"], THOUGHT,
                "Anthropic verifies the text against the signature"
            );
            Some((
                blocks[thinking]["signature"].as_str()?.to_string(),
                thinking < tool_use,
            ))
        }
        Protocol::OpenaiResponses => {
            let items = sent["input"].as_array()?;
            let reasoning = items.iter().position(|item| item["type"] == "reasoning")?;
            let call = items
                .iter()
                .position(|item| item["type"] == "function_call")?;
            Some((
                items[reasoning]["encrypted_content"].as_str()?.to_string(),
                reasoning < call,
            ))
        }
        Protocol::Gemini => {
            let parts = sent["contents"]
                .as_array()?
                .iter()
                .find(|content| content["role"] == "model")?["parts"]
                .as_array()?;
            let call = parts
                .iter()
                .find(|part| part.get("functionCall").is_some())?;
            Some((call["thoughtSignature"].as_str()?.to_string(), true))
        }
        // Chat Completions has no signatures.
        Protocol::OpenaiChat => None,
    }
}

/// Checks the second upstream request of a conversation: the tool call and
/// its result are there, and the reasoning signature is back as issued.
fn assert_replayed(label: &str, upstream: Protocol, harness: &Harness, signature: &str) {
    let recorded = harness.fake.last();
    let sent = support::decode_upstream(&recorded, upstream);
    let calls: Vec<_> = sent.messages.iter().flat_map(|m| m.tool_calls()).collect();
    assert_eq!(calls.len(), 1, "{label}: {sent:?}");
    assert_eq!(calls[0].name, "get_weather", "{label}");
    let results: Vec<_> = sent
        .messages
        .iter()
        .flat_map(|m| m.tool_results())
        .collect();
    assert_eq!(results.len(), 1, "{label}: {sent:?}");
    assert_eq!(results[0].call_id, calls[0].id, "{label}");

    let raw = String::from_utf8_lossy(&recorded.raw);
    assert!(
        !raw.contains("sy1."),
        "{label}: a wrapped signature reached the upstream"
    );
    assert!(
        !raw.contains("c3kxL"),
        "{label}: an armoured signature reached the upstream"
    );
    if upstream == Protocol::OpenaiChat {
        assert!(!raw.contains(signature), "{label}");
        return;
    }
    let (replayed, in_place) = replayed_signature(upstream, &recorded.body)
        .unwrap_or_else(|| panic!("{label}: no signed reasoning upstream: {}", recorded.body));
    assert_eq!(replayed.len(), signature.len(), "{label}: signature length");
    assert!(replayed == signature, "{label}: the signature changed");
    assert!(in_place, "{label}: the reasoning does not precede its call");
}

/// What every client must have been given in turn one.
fn assert_first_answer(label: &str, client: Protocol, response: &Response, model: &str) {
    assert_eq!(response.model, model, "{label}");
    assert_eq!(response.finish, FinishReason::ToolCalls, "{label}");
    let calls: Vec<_> = response.tool_calls().collect();
    assert_eq!(calls.len(), 1, "{label}: {response:?}");
    assert_eq!(calls[0].name, "get_weather", "{label}");
    let thought: String = response
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Reasoning(reasoning) => Some(reasoning.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thought, THOUGHT, "{label}: {client} sees the reasoning");
}

/// One cell: two turns of `client` against `upstream`.
async fn conversation(harness: &Harness, client: Protocol, upstream: Protocol, signature: &str) {
    let label = format!("{client} -> {upstream}");
    let (model, _, key) = route(upstream);
    let answer = || {
        Behaviour::Reply(
            Answer::tool("get_weather", json!({"city": "Paris"}))
                .with_reasoning(THOUGHT, signature),
        )
    };

    // Turn one, streamed: the reasoning and the call arrive.
    harness.fake.clear();
    harness.fake.script(key, [answer()]);
    let streamed = harness
        .ask_with(client, turn_one(client, model, true), model, true)
        .await;
    assert!(streamed.streamed, "{label}: {:?}", streamed.body);
    assert_first_answer(&label, client, &streamed.response(client), model);
    // The upstream was asked to reason.
    let asked = support::decode_upstream(&harness.fake.last(), upstream);
    assert!(
        asked.reasoning.as_ref().is_some_and(|r| r.depth.is_some()),
        "{label}: reasoning was not asked for upstream: {}",
        harness.fake.last().body
    );

    // Turn one, complete.
    harness.fake.clear();
    harness.fake.script(key, [answer()]);
    let first = turn_one(client, model, false);
    let output = harness.ask_with(client, first.clone(), model, false).await;
    assert_eq!(output.status, 200, "{label}: {:?}", output.body);
    assert_first_answer(&label, client, &output.response(client), model);
    if client.family() != upstream.family() {
        // Another vendor's signature is only ever handed out tagged with
        // its origin (and, for a Gemini client, armoured on top): no field
        // of the answer is the bare blob.
        let text = String::from_utf8_lossy(&output.body);
        assert!(
            !text.contains(&format!("\"{signature}")),
            "{label}: the client was handed another vendor's bare signature: {}",
            text.replace(signature, "<SIGNATURE>")
        );
        if text.contains(signature) {
            let tag = format!("sy1.{}.", upstream.tag());
            assert!(text.contains(&tag), "{label}: untagged");
        }
    }

    // Turn two: the echo.
    harness.fake.clear();
    let second = turn_two(client, &first, &output.json());
    let output = harness.ask_with(client, second, model, false).await;
    assert_eq!(output.status, 200, "{label}: {:?}", output.body);
    assert_eq!(
        output.response(client).text(),
        "Hello from the fake upstream",
        "{label}"
    );
    assert_eq!(harness.fake.count(), 1, "{label}");
    assert_replayed(&label, upstream, harness, signature);
}

/// One row of the matrix: `client` against every upstream protocol.
async fn row(client: Protocol) {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    // Longer than any limit a codec ever had on a signature field.
    let signature = signature(3_000);
    for upstream in PROTOCOLS {
        conversation(&harness, client, upstream, &signature).await;
    }
    support::eventually("gauges return to zero", || {
        let gauges = harness.gateway.telemetry().gauges();
        gauges.in_flight() == 0 && gauges.active_streams() == 0
    })
    .await;
}

#[tokio::test]
async fn signed_reasoning_survives_two_turns_of_a_chat_client() {
    row(Protocol::OpenaiChat).await;
}

#[tokio::test]
async fn signed_reasoning_survives_two_turns_of_a_responses_client() {
    row(Protocol::OpenaiResponses).await;
}

#[tokio::test]
async fn signed_reasoning_survives_two_turns_of_an_anthropic_client() {
    row(Protocol::Anthropic).await;
}

#[tokio::test]
async fn signed_reasoning_survives_two_turns_of_a_gemini_client() {
    row(Protocol::Gemini).await;
}

/// OpenAI's `encrypted_content` is the longest of them all; through a
/// Gemini client it comes back to the Responses upstream as issued.
#[tokio::test]
async fn a_gemini_client_replays_20_000_characters_of_encrypted_reasoning() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    conversation(
        &harness,
        Protocol::Gemini,
        Protocol::OpenaiResponses,
        &signature(20_000),
    )
    .await;
}

/// The parts of a streamed Gemini answer as a client assembles them.
fn gemini_stream_parts(output: &Output) -> Value {
    assert!(output.streamed, "{:?}", output.body);
    let mut parts = Vec::new();
    for event in &output.events {
        let Ok(chunk) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        if let Some(more) = chunk["candidates"][0]["content"]["parts"].as_array() {
            parts.extend(more.iter().cloned());
        }
    }
    Value::Array(parts)
}

/// A Gemini client on an Anthropic upstream with thinking and tool use: the
/// case a length limit on `thoughtSignature` used to break on turn two.
#[tokio::test]
async fn a_gemini_client_replays_a_20_000_character_anthropic_signature() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let signature = signature(20_000);
    for stream in [false, true] {
        let label = format!("stream={stream}");
        harness.fake.clear();
        harness.fake.script(
            "key-anthropic-1",
            [Behaviour::Reply(
                Answer::tool("get_weather", json!({"city": "Paris"}))
                    .with_reasoning(THOUGHT, &signature),
            )],
        );
        let mut request = turn_one(Protocol::Gemini, "m-anthropic", stream);
        let output = harness
            .ask_with(Protocol::Gemini, request.clone(), "m-anthropic", stream)
            .await;
        assert_eq!(output.status, 200, "{label}: {:?}", output.body);
        // Thinking was switched on upstream, with the client's budget.
        let asked = harness.fake.last().body;
        assert_eq!(asked["thinking"]["type"], "enabled", "{label}: {asked}");
        assert_eq!(asked["thinking"]["budget_tokens"], 2048, "{label}");

        let parts = if stream {
            gemini_stream_parts(&output)
        } else {
            output.json()["candidates"][0]["content"]["parts"].clone()
        };
        let text = parts.to_string();
        assert!(text.contains(THOUGHT), "{label}: {text}");
        assert!(
            !text.contains(&signature),
            "{label}: the client is handed the wrapped form, not Anthropic's own bytes"
        );
        let carried: Vec<&str> = parts
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|part| part["thoughtSignature"].as_str())
            .collect();
        assert_eq!(carried.len(), 1, "{label}: {text}");
        assert!(carried[0].len() > 20_000, "{label}");

        // Turn two: the model turn exactly as received, and the result.
        push_gemini_turn(&mut request, parts);
        harness.fake.clear();
        let output = harness
            .ask_with(Protocol::Gemini, request, "m-anthropic", stream)
            .await;
        assert_eq!(output.status, 200, "{label}: {:?}", output.body);
        assert_eq!(
            output.response(Protocol::Gemini).text(),
            "Hello from the fake upstream",
            "{label}"
        );

        let upstream = harness.fake.last().body;
        let messages = upstream["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3, "{label}: {upstream}");
        let blocks = messages[1]["content"].as_array().unwrap();
        assert_eq!(messages[1]["role"], "assistant", "{label}");
        assert_eq!(blocks.len(), 2, "{label}: {}", messages[1]);
        assert_eq!(blocks[0]["type"], "thinking", "{label}");
        assert_eq!(blocks[0]["thinking"], THOUGHT, "{label}");
        assert!(
            blocks[0]["signature"].as_str() == Some(signature.as_str()),
            "{label}: the signature changed on its way through the client"
        );
        assert_eq!(blocks[1]["type"], "tool_use", "{label}");
        assert_eq!(blocks[1]["name"], "get_weather", "{label}");
        assert_eq!(blocks[1]["input"], json!({"city": "Paris"}), "{label}");
        let result = &messages[2]["content"][0];
        assert_eq!(result["type"], "tool_result", "{label}");
        assert_eq!(result["tool_use_id"], blocks[1]["id"], "{label}");
        // Thinking stays on for the turn that replays it.
        assert_eq!(upstream["thinking"]["type"], "enabled", "{label}");
    }
}

/// The same through the codecs alone, to the protocol the blob came from:
/// `encode_response` for a Gemini client, the client's echo,
/// `decode_request`, and `encode_request` for the vendor that issued it.
#[test]
fn a_long_foreign_signature_returns_to_its_origin_protocol() {
    let gemini = codec(Protocol::Gemini);
    let signature = signature(20_000);
    for origin in [Protocol::Anthropic, Protocol::OpenaiResponses] {
        let mut response = Response::new("resp-1", "upstream-model");
        response.parts = vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: THOUGHT.into(),
                signature: Some(Signature::new(origin, signature.clone())),
                redacted: false,
            }),
            Part::tool_call("call_1", "get_weather", r#"{"city":"Paris"}"#),
        ];
        let answer = gemini
            .encode_response(&response, &ClientCtx::new("m"))
            .expect("the response encodes");
        let mut echo = json!({
            "contents": [{"role": "user", "parts": [{"text": "weather in Paris?"}]}],
            "tools": tool_declaration(Protocol::Gemini, "get_weather"),
            "generationConfig": {"thinkingConfig": {"thinkingBudget": 2048, "includeThoughts": true}}
        });
        push_gemini_turn(
            &mut echo,
            answer["candidates"][0]["content"]["parts"].clone(),
        );
        let request: Request = gemini
            .decode_request(
                &echo,
                &RequestPath {
                    model: Some("m"),
                    stream: Some(false),
                },
            )
            .expect("the echo decodes");

        let upstream = codec(origin)
            .encode_request(&request, &UpstreamCtx::default())
            .expect("the request encodes for its origin");
        let (replayed, in_place) = replayed_signature(origin, &upstream)
            .unwrap_or_else(|| panic!("{origin}: no signed reasoning in {upstream}"));
        assert!(replayed == signature, "{origin}: the signature changed");
        assert!(in_place, "{origin}");

        // And to no other vendor.
        for other in PROTOCOLS {
            if other.family() == origin.family() {
                continue;
            }
            let body = codec(other)
                .encode_request(&request, &UpstreamCtx::default())
                .expect("the request encodes");
            assert!(
                !body.to_string().contains(&signature),
                "{origin}'s signature was encoded for {other}"
            );
        }
    }
}
