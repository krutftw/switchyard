//! Review evidence: the `generateContentRequest` form of a `countTokens`
//! request is not understood on the client side.
//!
//! `countTokens` takes EITHER `{"contents": [...]}` OR
//! `{"generateContentRequest": {"model": "models/<id>", "contents": [...],
//! "systemInstruction": ..., "tools": ...}}` (notes 15 section 6.7; it is the
//! only way to count system instructions and tools, and it is the form the
//! legacy Google SDKs always send). The codec itself *writes* that form in
//! `encode_count_request`, but:
//!
//! * `decode_request` rejects it with "`contents` is required", so such a
//!   count can never be translated to another protocol's counting endpoint
//!   or estimated locally;
//! * `set_request_model` leaves `generateContentRequest.model` alone, so on
//!   the passthrough path the upstream receives the client-facing alias in
//!   the body next to the real model id in the URL.

use pretty_assertions::assert_eq;
use serde_json::json;
use switchyard_codec_gemini::GeminiCodec;
use switchyard_core::ir::{Part, Role, Tool};
use switchyard_core::{Codec, RequestPath};

fn count_body() -> serde_json::Value {
    json!({
        "generateContentRequest": {
            "model": "models/fast",
            "contents": [{"role": "user", "parts": [{"text": "how many tokens?"}]}],
            "systemInstruction": {"parts": [{"text": "be brief"}]},
            "tools": [{"functionDeclarations": [
                {"name": "lookup", "description": "look something up",
                 "parametersJsonSchema": {"type": "object", "properties": {}}}
            ]}]
        }
    })
}

#[test]
fn decode_request_understands_the_generate_content_request_wrapper() {
    let path = RequestPath {
        model: Some("fast"),
        stream: Some(false),
    };
    let request = GeminiCodec
        .decode_request(&count_body(), &path)
        .expect("the wrapped countTokens form is a valid Gemini request body");
    assert_eq!(request.model, "fast");
    assert_eq!(request.system, vec![Part::text("be brief")]);
    assert_eq!(request.messages.len(), 1);
    assert_eq!(request.messages[0].role, Role::User);
    assert_eq!(request.messages[0].text(), "how many tokens?");
    assert_eq!(request.tools.len(), 1);
    assert_eq!(request.tools[0].name(), Some("lookup"));
    assert!(matches!(request.tools[0], Tool::Function(_)));
}

#[test]
fn set_request_model_keeps_the_wrapped_model_consistent() {
    // The client asked for its alias `fast`; the gateway routes it to
    // `gemini-2.5-flash` and calls `.../models/gemini-2.5-flash:countTokens`.
    let mut body = count_body();
    GeminiCodec.set_request_model(&mut body, "gemini-2.5-flash");
    assert_eq!(
        body["generateContentRequest"]["model"],
        json!("models/gemini-2.5-flash"),
        "a body `model` field must follow the routed model"
    );
    // Nothing else moves, and no top-level `model` is invented.
    assert!(body.get("model").is_none());
    assert_eq!(
        body["generateContentRequest"]["contents"],
        count_body()["generateContentRequest"]["contents"]
    );
}
