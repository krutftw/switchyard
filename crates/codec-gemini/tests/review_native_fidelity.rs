//! Review evidence: details of a native Gemini request that are damaged when
//! it goes through the canonical model (Gemini client -> decode -> encode for
//! a Gemini upstream, which is what happens whenever the history carries a
//! wrapped foreign signature, e.g. after a fail-over from another provider;
//! and Gemini client -> another vendor).

use pretty_assertions::assert_eq;
use serde_json::json;
use switchyard_codec_gemini::GeminiCodec;
use switchyard_core::ir::Tool;
use switchyard_core::{Codec, RequestPath, UpstreamCtx};

fn path() -> RequestPath<'static> {
    RequestPath {
        model: Some("gemini-2.5-pro"),
        stream: Some(false),
    }
}

/// `fileData` without a `mimeType` (the documented way to pass a YouTube URL)
/// comes back out with an invented `"mimeType": "application/pdf"`, which
/// tells Gemini to read a video page as a PDF. A provider file handle in the
/// same position is re-emitted without a MIME type; a plain URL must be
/// treated the same way when nothing is known about its type.
#[test]
fn file_data_without_a_mime_type_does_not_become_a_pdf() {
    let body = json!({"contents": [{"role": "user", "parts": [
        {"fileData": {"fileUri": "https://www.youtube.com/watch?v=9hE5-98ZeCg"}},
        {"text": "summarise this video"}
    ]}]});
    let request = GeminiCodec
        .decode_request(&body, &path())
        .expect("request decodes");
    let encoded = GeminiCodec
        .encode_request(&request, &UpstreamCtx::default())
        .expect("request encodes");
    assert_eq!(
        encoded["contents"],
        json!([{"role": "user", "parts": [
            {"fileData": {"fileUri": "https://www.youtube.com/watch?v=9hE5-98ZeCg"}},
            {"text": "summarise this video"}
        ]}])
    );
}

/// Gemini's `Schema` message declares `minItems`, `maxItems`, `minLength`,
/// `maxLength`, `minProperties` and `maxProperties` as `int64`, which proto3
/// JSON renders as **strings** (`"minItems": "1"`; the REST reference lists
/// them as "string (int64 format)" and Google's JS SDK types them as
/// `string`). `from_gemini_schema` is documented as turning the dialect into
/// standard JSON Schema, where these keywords must be numbers: Anthropic and
/// OpenAI reject `"minItems": "1"` as an invalid schema.
#[test]
fn int64_string_constraints_of_the_gemini_dialect_become_numbers() {
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "x"}]}],
        "tools": [{"functionDeclarations": [{"name": "tag", "parameters": {
            "type": "OBJECT",
            "minProperties": "1",
            "properties": {"tags": {
                "type": "ARRAY", "minItems": "1", "maxItems": "3",
                "items": {"type": "STRING", "minLength": "2", "maxLength": "10"}
            }},
            "required": ["tags"]
        }}]}]
    });
    let request = GeminiCodec
        .decode_request(&body, &path())
        .expect("request decodes");
    let Tool::Function(function) = &request.tools[0] else {
        panic!("expected a function tool");
    };
    assert_eq!(
        function.parameters,
        json!({
            "type": "object",
            "minProperties": 1,
            "properties": {"tags": {
                "type": "array", "minItems": 1, "maxItems": 3,
                "items": {"type": "string", "minLength": 2, "maxLength": 10}
            }},
            "required": ["tags"]
        })
    );
}
