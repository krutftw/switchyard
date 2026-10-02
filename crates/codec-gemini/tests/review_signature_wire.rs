//! Review evidence: a foreign signature handed to a Gemini client is not a
//! valid `thoughtSignature`.
//!
//! `Part.thoughtSignature` is a protobuf `bytes` field: on the wire it is a
//! base64 string (notes 15 section 6.2 / 6.5: "`thoughtSignature:
//! string(base64)`"). Typed Gemini SDKs decode it while parsing the response
//! (the Go SDK declares `ThoughtSignature []byte`; `encoding/json` fails the
//! whole response on "illegal base64 data"). `encode_response` and the stream
//! encoder write `sig::encode_for_client` output verbatim, which for a blob
//! of another vendor is `sy1.<tag>.<blob>` — the dots make it invalid base64,
//! so a Gemini-protocol client cannot even parse an answer that carries
//! Anthropic thinking or OpenAI encrypted reasoning.
//!
//! The two documented bypass literals show the constraint: both
//! (`skip_thought_signature_validator`, `context_engineering_is_the_way_to_go`)
//! are valid base64url strings.
//!
//! Note for the fixer: the `sy1.` wrapping is mandated by DESIGN.md / the
//! core `sig` module, so this is a conflict between that contract and the
//! Gemini wire format. It can be resolved inside this codec by armouring the
//! wrapped string (e.g. standard base64 of `sy1.<tag>.<blob>`) when writing
//! for a Gemini client and undoing that in `decode_request`; the passthrough
//! path (`prepare_passthrough`) then has to drop such armoured foreign blobs
//! itself, because `sig::contains_wrapped` no longer sees them. If that is
//! not acceptable the contract in core has to change — report it rather than
//! silently keeping a value typed clients cannot read.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_gemini::GeminiCodec;
use switchyard_core::ir::{Part, Reasoning, Response, Signature};
use switchyard_core::stream::response_to_events;
use switchyard_core::{ClientCtx, Codec, Protocol, RequestPath};

/// Characters of the standard and URL-safe base64 alphabets plus padding.
fn is_base64(text: &str) -> bool {
    let body = text.trim_end_matches('=');
    !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'-' | b'_'))
}

fn anthropic_signed_response() -> Response {
    let mut response = Response::new("resp-1", "claude-sonnet-4-5");
    response.parts = vec![
        Part::Reasoning(Reasoning {
            id: None,
            text: "let me think".into(),
            signature: Some(Signature::new(
                Protocol::Anthropic,
                "EuYBCkYIBxgCKkD+abc/def==",
            )),
            redacted: false,
        }),
        Part::text("answer"),
    ];
    response
}

fn signatures_in(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "thoughtSignature"
                    && let Some(text) = child.as_str()
                {
                    out.push(text.to_string());
                }
                signatures_in(child, out);
            }
        }
        Value::Array(list) => list.iter().for_each(|child| signatures_in(child, out)),
        _ => {}
    }
}

#[test]
fn bypass_literals_are_base64_but_wrapped_signatures_are_not() {
    // Sanity check of the helper against values Google itself documents.
    assert!(is_base64("skip_thought_signature_validator"));
    assert!(is_base64("context_engineering_is_the_way_to_go"));
    assert!(is_base64("EuYBCkYIBxgCKkD+abc/def=="));
    assert!(!is_base64("sy1.a.EuYBCkYIBxgCKkD+abc/def=="));
}

#[test]
fn foreign_signature_in_a_response_is_valid_base64_for_gemini_clients() {
    let body = GeminiCodec
        .encode_response(&anthropic_signed_response(), &ClientCtx::new("my-model"))
        .expect("response encodes");
    let mut signatures = Vec::new();
    signatures_in(&body, &mut signatures);
    assert_eq!(signatures.len(), 1, "the signature is delivered: {body}");
    assert!(
        is_base64(&signatures[0]),
        "`thoughtSignature` is a base64 `bytes` field, got {:?}",
        signatures[0]
    );

    // ... and what the client sends back is still recognised as Anthropic's.
    let replay = json!({"contents": [
        {"role": "user", "parts": [{"text": "hi"}]},
        {"role": "model", "parts": [
            {"text": "let me think", "thought": true, "thoughtSignature": signatures[0]},
            {"text": "answer"}]},
        {"role": "user", "parts": [{"text": "more"}]}
    ]});
    let path = RequestPath {
        model: Some("my-model"),
        stream: Some(false),
    };
    let request = GeminiCodec
        .decode_request(&replay, &path)
        .expect("replay decodes");
    let Part::Reasoning(reasoning) = &request.messages[1].parts[0] else {
        panic!("expected reasoning, got {:?}", request.messages[1].parts[0]);
    };
    assert_eq!(
        reasoning.signature,
        Some(Signature::new(
            Protocol::Anthropic,
            "EuYBCkYIBxgCKkD+abc/def=="
        ))
    );
}

#[test]
fn foreign_signature_in_a_stream_is_valid_base64_for_gemini_clients() {
    let mut encoder = GeminiCodec.stream_encoder(&ClientCtx::new("my-model"));
    let mut wire = Vec::new();
    for event in response_to_events(&anthropic_signed_response()) {
        wire.extend(encoder.encode(&event));
    }
    wire.extend(encoder.finish());
    let mut signatures = Vec::new();
    for event in &wire {
        let payload: Value = serde_json::from_str(&event.data).expect("chunk is JSON");
        signatures_in(&payload, &mut signatures);
    }
    assert_eq!(signatures.len(), 1, "the signature is delivered: {wire:?}");
    assert!(
        is_base64(&signatures[0]),
        "`thoughtSignature` is a base64 `bytes` field, got {:?}",
        signatures[0]
    );
}
