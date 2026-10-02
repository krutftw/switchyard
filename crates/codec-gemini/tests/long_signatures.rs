//! Signatures of real-world size through a Gemini client.
//!
//! An Anthropic thinking signature or an OpenAI `encrypted_content` blob is
//! kilobytes long, and it reaches a Gemini-protocol client tagged
//! (`sy1.<tag>.<blob>`) and base64-armoured in `thoughtSignature`, a third
//! longer still. The client echoes it on the next turn and the request
//! decoder has to hand the original blob, byte for byte, back to the
//! pipeline — or the vendor that issued it rejects the conversation on turn
//! two. There is no length at which that may stop working: these tests go
//! to 20,000 characters (and once to a full mebibyte).

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_gemini::GeminiCodec;
use switchyard_core::ir::{Part, Reasoning, Request, Response, Signature};
use switchyard_core::stream::response_to_events;
use switchyard_core::{ClientCtx, Codec, Protocol, RequestPath, UpstreamCtx};

/// A blob of exactly `len` characters that looks like the real thing:
/// base64 text using the whole standard alphabet (`+` and `/` included),
/// different at every position so that truncation, reordering or a dropped
/// chunk cannot go unnoticed.
fn blob(len: usize) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut state = 0x9E37_79B9_u32;
    (0..len)
        .map(|_| {
            // A small linear congruential generator: deterministic, and
            // good enough to never repeat a long run.
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ALPHABET[(state >> 26) as usize] as char
        })
        .collect()
}

const PATH: RequestPath<'static> = RequestPath {
    model: Some("my-model"),
    stream: Some(false),
};

/// A response of another vendor: signed reasoning, then a tool call.
fn foreign_response(origin: Protocol, signature: &str, redacted: bool) -> Response {
    let mut response = Response::new("resp-1", "upstream-model");
    response.parts = vec![
        Part::Reasoning(Reasoning {
            id: None,
            text: if redacted {
                String::new()
            } else {
                "The weather tool will know.".into()
            },
            signature: Some(Signature::new(origin, signature)),
            redacted,
        }),
        Part::tool_call("call_1", "get_weather", r#"{"city":"Paris"}"#),
    ];
    response
}

/// The next request of a client that was answered with `model_parts`: the
/// model turn echoed exactly as received, then the tool's result.
fn echo(model_parts: Value) -> Value {
    json!({
        "contents": [
            {"role": "user", "parts": [{"text": "weather in Paris?"}]},
            {"role": "model", "parts": model_parts},
            {"role": "user", "parts": [{"functionResponse": {
                "name": "get_weather", "response": {"temperature": "18 degrees"}
            }}]}
        ],
        "tools": [{"functionDeclarations": [{
            "name": "get_weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
        }]}],
        "generationConfig": {"thinkingConfig": {"thinkingBudget": 2048, "includeThoughts": true}}
    })
}

/// The parts of a complete response body.
fn parts_of(body: &Value) -> Value {
    let parts = body["candidates"][0]["content"]["parts"].clone();
    assert!(parts.is_array(), "no parts in {body}");
    parts
}

/// The parts of a streamed response, as a client assembles them: every
/// chunk's parts, in order.
fn streamed_parts(response: &Response) -> Value {
    let mut encoder = GeminiCodec.stream_encoder(&ClientCtx::new("my-model"));
    let mut wire = Vec::new();
    for event in response_to_events(response) {
        wire.extend(encoder.encode(&event));
    }
    wire.extend(encoder.finish());
    let mut parts = Vec::new();
    for event in &wire {
        let chunk: Value = serde_json::from_str(&event.data).expect("a chunk is JSON");
        if let Some(more) = chunk["candidates"][0]["content"]["parts"].as_array() {
            parts.extend(more.iter().cloned());
        }
    }
    Value::Array(parts)
}

/// Every `thoughtSignature` in `value`.
fn signatures_in(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                match child.as_str() {
                    Some(text) if key == "thoughtSignature" => out.push(text.to_string()),
                    _ => signatures_in(child, out),
                }
            }
        }
        Value::Array(list) => list.iter().for_each(|child| signatures_in(child, out)),
        _ => {}
    }
}

/// The reasoning parts of the model turn of a decoded request.
fn reasoning_of(request: &Request) -> Vec<&Reasoning> {
    request.messages[1]
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Reasoning(reasoning) => Some(reasoning),
            _ => None,
        })
        .collect()
}

/// Checks that the echo of `model_parts` decodes to reasoning signed with
/// exactly `signature` of `origin`, in front of the tool call.
fn assert_survives(model_parts: Value, origin: Protocol, signature: &str, redacted: bool) {
    let label = format!("{origin}, {} characters", signature.len());
    let mut delivered = Vec::new();
    signatures_in(&model_parts, &mut delivered);
    assert_eq!(delivered.len(), 1, "{label}: one signature is delivered");
    assert!(
        delivered[0].len() > signature.len(),
        "{label}: the armoured form is longer than the blob"
    );
    assert!(
        !delivered[0].contains(signature),
        "{label}: the client never sees the vendor's own bytes"
    );
    assert!(
        delivered[0]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')),
        "{label}: `thoughtSignature` is a base64 `bytes` field"
    );

    let request = GeminiCodec
        .decode_request(&echo(model_parts), &PATH)
        .unwrap_or_else(|error| panic!("{label}: the echo must decode: {error}"));
    let reasoning = reasoning_of(&request);
    assert_eq!(reasoning.len(), 1, "{label}: {:?}", request.messages[1]);
    let found = reasoning[0]
        .signature
        .as_ref()
        .unwrap_or_else(|| panic!("{label}: the signature is gone"));
    assert_eq!(found.origin, origin, "{label}");
    assert_eq!(found.data.len(), signature.len(), "{label}: length");
    assert!(found.data == signature, "{label}: the blob changed");
    assert_eq!(reasoning[0].redacted, redacted, "{label}");
    // The reasoning still stands in front of the call it led to.
    let position = |wanted: fn(&Part) -> bool| request.messages[1].parts.iter().position(wanted);
    let thought = position(|part| matches!(part, Part::Reasoning(_)));
    let call = position(|part| matches!(part, Part::ToolCall(_)));
    assert!(
        thought < call && call.is_some(),
        "{label}: {:?}",
        request.messages[1]
    );
    // The tool result still answers the call.
    assert_eq!(request.messages[2].tool_results().count(), 1, "{label}");
}

#[test]
fn a_20_000_character_foreign_signature_survives_the_clients_echo() {
    let signature = blob(20_000);
    for origin in [Protocol::Anthropic, Protocol::OpenaiResponses] {
        let response = foreign_response(origin, &signature, false);
        let body = GeminiCodec
            .encode_response(&response, &ClientCtx::new("my-model"))
            .expect("the response encodes");
        assert_survives(parts_of(&body), origin, &signature, false);
    }
}

#[test]
fn a_20_000_character_foreign_signature_survives_a_streamed_turn() {
    let signature = blob(20_000);
    for origin in [Protocol::Anthropic, Protocol::OpenaiResponses] {
        let response = foreign_response(origin, &signature, false);
        assert_survives(streamed_parts(&response), origin, &signature, false);
    }
}

#[test]
fn withheld_reasoning_of_that_size_survives_too() {
    // Anthropic `redacted_thinking`: the blob *is* the reasoning.
    let payload = blob(20_000);
    let response = foreign_response(Protocol::Anthropic, &payload, true);
    let body = GeminiCodec
        .encode_response(&response, &ClientCtx::new("my-model"))
        .expect("the response encodes");
    assert_survives(parts_of(&body), Protocol::Anthropic, &payload, true);
    assert_survives(
        streamed_parts(&response),
        Protocol::Anthropic,
        &payload,
        true,
    );
}

#[test]
fn signature_lengths_around_every_base64_boundary_survive() {
    // The armour pads differently for each length modulo three, and blocks
    // of 512 and 1024 are where a fixed-size buffer would have ended.
    for len in [
        30, 31, 32, 383, 384, 385, 511, 512, 513, 682, 683, 684, 1023, 1024, 1025, 4095, 4096,
        4097, 19_999, 20_001, 65_535, 65_536, 65_537,
    ] {
        let signature = blob(len);
        let response = foreign_response(Protocol::Anthropic, &signature, false);
        let body = GeminiCodec
            .encode_response(&response, &ClientCtx::new("my-model"))
            .expect("the response encodes");
        assert_survives(parts_of(&body), Protocol::Anthropic, &signature, false);
    }
}

#[test]
fn a_mebibyte_of_signature_survives() {
    let signature = blob(1024 * 1024);
    for origin in [Protocol::Anthropic, Protocol::OpenaiResponses] {
        let response = foreign_response(origin, &signature, false);
        let body = GeminiCodec
            .encode_response(&response, &ClientCtx::new("my-model"))
            .expect("the response encodes");
        assert_survives(parts_of(&body), origin, &signature, false);
    }
}

#[test]
fn a_client_that_re_encodes_the_bytes_its_own_way_changes_nothing() {
    // Typed SDKs parse `thoughtSignature` into bytes and write them back
    // with their own base64 flavour: URL-safe, unpadded.
    let signature = blob(20_000);
    let response = foreign_response(Protocol::OpenaiResponses, &signature, false);
    let body = GeminiCodec
        .encode_response(&response, &ClientCtx::new("my-model"))
        .expect("the response encodes");
    let mut parts = parts_of(&body);
    let mut rewritten = 0;
    for part in parts.as_array_mut().unwrap() {
        let Some(armoured) = part.get("thoughtSignature").and_then(Value::as_str) else {
            continue;
        };
        let bytes = STANDARD.decode(armoured).expect("standard base64");
        part["thoughtSignature"] = json!(URL_SAFE_NO_PAD.encode(bytes));
        rewritten += 1;
    }
    assert_eq!(rewritten, 1);

    let request = GeminiCodec
        .decode_request(&echo(parts), &PATH)
        .expect("the echo decodes");
    let reasoning = reasoning_of(&request);
    assert_eq!(reasoning.len(), 1);
    assert_eq!(
        reasoning[0].signature,
        Some(Signature::new(Protocol::OpenaiResponses, signature))
    );
}

#[test]
fn a_long_foreign_signature_never_reaches_a_gemini_upstream() {
    let signature = blob(20_000);
    let response = foreign_response(Protocol::Anthropic, &signature, false);
    let body = GeminiCodec
        .encode_response(&response, &ClientCtx::new("my-model"))
        .expect("the response encodes");
    let parts = parts_of(&body);
    let mut delivered = Vec::new();
    signatures_in(&parts, &mut delivered);
    let armoured = delivered.pop().expect("a signature");
    let turn_two = echo(parts);

    // Translated: the decoded request, encoded for a Gemini upstream.
    let request = GeminiCodec
        .decode_request(&turn_two, &PATH)
        .expect("the echo decodes");
    let upstream = GeminiCodec
        .encode_request(&request, &UpstreamCtx::default())
        .expect("the request encodes");
    let text = upstream.to_string();
    assert!(
        !text.contains(&signature),
        "Anthropic's blob went to Gemini"
    );
    assert!(
        !text.contains(&armoured),
        "the armoured blob went to Gemini"
    );
    // The conversation itself is intact.
    assert_eq!(
        upstream["contents"].as_array().map(Vec::len),
        Some(3),
        "{upstream}"
    );
    assert!(text.contains("functionCall") && text.contains("functionResponse"));

    // Forwarded: the body as the client sent it, repaired in place.
    let mut forwarded = turn_two;
    GeminiCodec.prepare_passthrough(&mut forwarded, false, &UpstreamCtx::default());
    let text = forwarded.to_string();
    assert!(!text.contains(&armoured), "the armoured blob was forwarded");
    assert!(text.contains("functionCall") && text.contains("functionResponse"));
}

#[test]
fn a_long_native_signature_goes_back_to_gemini_untouched() {
    // Gemini's own signatures are not short either.
    let signature = blob(20_000);
    let turn_two = echo(json!([
        {"text": "The weather tool will know.", "thought": true},
        {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}},
         "thoughtSignature": signature}
    ]));

    let request = GeminiCodec
        .decode_request(&turn_two, &PATH)
        .expect("the request decodes");
    let call = request.messages[1]
        .tool_calls()
        .next()
        .expect("the tool call");
    assert_eq!(
        call.signature,
        Some(Signature::new(Protocol::Gemini, signature.clone()))
    );
    let upstream = GeminiCodec
        .encode_request(&request, &UpstreamCtx::default())
        .expect("the request encodes");
    let mut sent = Vec::new();
    signatures_in(&upstream, &mut sent);
    assert_eq!(sent, vec![signature.clone()]);

    let mut forwarded = turn_two;
    GeminiCodec.prepare_passthrough(&mut forwarded, false, &UpstreamCtx::default());
    let mut sent = Vec::new();
    signatures_in(&forwarded, &mut sent);
    assert_eq!(sent, vec![signature]);
}
