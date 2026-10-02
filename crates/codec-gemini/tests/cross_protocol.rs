//! Regression tests for defects found by the cross-protocol matrix
//! (`crates/codecs/tests`): what a Gemini client is shown for an answer
//! another protocol's upstream gave, and what becomes of the parts of a
//! Gemini client's request that other protocols cannot express.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::sync::Arc;
use switchyard_codec_gemini::GeminiCodec;
use switchyard_core::ir::{
    FinishReason, Part, RefusalPart, Request, Response, ResponseFormat, Tool, ToolChoice,
};
use switchyard_core::stream::response_to_events;
use switchyard_core::{ClientCtx, Codec, RequestPath, UpstreamCtx};

fn function_call_names(payload: &Value) -> Vec<Value> {
    payload["candidates"][0]["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("functionCall"))
        .map(|call| call["name"].clone())
        .collect()
}

/// Gemini function names may contain dots and colons (`mcp.files:read-file`
/// is how MCP tools are commonly declared). OpenAI and Anthropic refuse
/// both, so their encoders declare the function as `mcp_files_read-file` and
/// the model calls it by that name. The Gemini client was handed that name
/// back, which is not a function it declared.
#[test]
fn function_calls_come_back_under_the_names_the_client_declared() {
    let ctx = ClientCtx::new("gemini-2.5-pro").with_request(Arc::new(json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "tools": [
            {"functionDeclarations": [
                {"name": "mcp.files:read-file", "parameters": {"type": "OBJECT"}},
                {"name": "getWeather", "parameters": {"type": "OBJECT"}}
            ]},
            {"function_declarations": [{"name": "a.b"}, {"name": "a:b"}]}
        ]
    })));
    for (upstream_name, declared) in [
        ("mcp_files_read-file", "mcp.files:read-file"),
        ("mcp.files:read-file", "mcp.files:read-file"),
        ("getWeather", "getWeather"),
        // Two declarations fit: not guessed at.
        ("a_b", "a_b"),
        ("unknown_tool", "unknown_tool"),
    ] {
        let mut response = Response::new("chatcmpl-1", "upstream-model");
        response.parts = vec![Part::tool_call(
            "call_1",
            upstream_name,
            r#"{"path":"/tmp"}"#,
        )];
        response.finish = FinishReason::ToolCalls;

        let body = GeminiCodec
            .encode_response(&response, &ctx)
            .expect("encodes");
        assert_eq!(function_call_names(&body), [json!(declared)]);

        let mut encoder = GeminiCodec.stream_encoder(&ctx);
        let mut wire = Vec::new();
        for event in response_to_events(&response) {
            wire.extend(encoder.encode(&event));
        }
        wire.extend(encoder.finish());
        let streamed: Vec<Value> = wire
            .iter()
            .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok())
            .flat_map(|chunk| function_call_names(&chunk))
            .collect();
        assert_eq!(streamed, [json!(declared)], "stream, {upstream_name}");
    }
}

fn decode(body: &Value) -> Request {
    GeminiCodec
        .decode_request(
            body,
            &RequestPath {
                model: Some("gemini-2.5-pro"),
                stream: Some(false),
            },
        )
        .expect("decodes")
}

/// `allowedFunctionNames` with several names restricts the model to some of
/// the declared functions. Only the mode used to survive decoding, so every
/// other upstream was offered all functions with "call any of them" and the
/// model could call exactly the ones the client had excluded.
#[test]
fn several_allowed_function_names_narrow_the_tool_list() {
    let request = decode(&json!({
        "contents": [{"role": "user", "parts": [{"text": "Confirm or cancel the order."}]}],
        "tools": [{"functionDeclarations": [
            {"name": "confirm_order"}, {"name": "cancel_order"}, {"name": "delete_account"}
        ]}],
        "toolConfig": {"functionCallingConfig": {
            "mode": "ANY", "allowedFunctionNames": ["confirm_order", "cancel_order"]
        }}
    }));
    assert_eq!(request.tool_choice, Some(ToolChoice::Required));
    let names: Vec<Option<&str>> = request.tools.iter().map(Tool::name).collect();
    assert_eq!(names, vec![Some("confirm_order"), Some("cancel_order")]);

    // A Gemini upstream gets the client's own `toolConfig` back, over the
    // functions that are still declared.
    let body = GeminiCodec
        .encode_request(&request, &UpstreamCtx::default())
        .expect("encodes");
    assert_eq!(
        body["toolConfig"],
        json!({"functionCallingConfig": {
            "mode": "ANY", "allowedFunctionNames": ["confirm_order", "cancel_order"]
        }})
    );
    let declared: Vec<&str> = body["tools"][0]["functionDeclarations"]
        .as_array()
        .expect("declarations")
        .iter()
        .filter_map(|declaration| declaration["name"].as_str())
        .collect();
    assert_eq!(declared, ["confirm_order", "cancel_order"]);
}

/// `responseMimeType: "text/x.enum"` answers with one bare enum value, not
/// with JSON. When such a request was re-encoded for a Gemini upstream (a
/// same-protocol request that carries another vendor's wrapped signature is
/// not forwarded verbatim) it came out as JSON mode with a
/// `responseJsonSchema`, and the answer as a quoted string.
#[test]
fn an_enum_output_mode_survives_re_encoding_for_gemini() {
    let config = json!({
        "responseMimeType": "text/x.enum",
        "responseSchema": {"type": "STRING", "enum": ["Percussion", "String", "Woodwind"]}
    });
    let request = decode(&json!({
        "contents": [{"role": "user", "parts": [{"text": "What is an oboe?"}]}],
        "generationConfig": config
    }));
    let body = GeminiCodec
        .encode_request(&request, &UpstreamCtx::default())
        .expect("encodes");
    assert_eq!(body["generationConfig"], config);

    // Other protocols are given what they can express: the schema.
    assert_eq!(
        request.response_format,
        Some(ResponseFormat::JsonSchema {
            name: None,
            description: None,
            schema: json!({"type": "string", "enum": ["Percussion", "String", "Woodwind"]}),
            strict: None,
        })
    );
    // JSON mode itself is untouched by this.
    let json_mode = json!({
        "responseMimeType": "application/json",
        "responseSchema": {"type": "OBJECT", "properties": {"kind": {"type": "STRING"}}}
    });
    let request = decode(&json!({
        "contents": [{"role": "user", "parts": [{"text": "What is an oboe?"}]}],
        "generationConfig": json_mode
    }));
    let body = GeminiCodec
        .encode_request(&request, &UpstreamCtx::default())
        .expect("encodes");
    assert_eq!(
        body["generationConfig"],
        json!({
            "responseMimeType": "application/json",
            "responseJsonSchema": {"type": "object", "properties": {"kind": {"type": "string"}}}
        })
    );
}

/// A refusal the model wrote out comes from a Chat Completions upstream as
/// `stop` + `message.refusal` and from a Responses upstream as a
/// `completed` response with a refusal part, which decode to `Stop` and
/// `Refusal`. A Gemini client was told `STOP` for the first and `SAFETY`
/// for the second: two outcomes for one model behaviour.
#[test]
fn a_written_out_refusal_reads_the_same_whichever_upstream_gave_it() {
    let ctx = ClientCtx::new("gemini-2.5-pro");
    for finish in [FinishReason::Stop, FinishReason::Refusal] {
        let mut response = Response::new("resp_1", "upstream-model");
        response.parts = vec![Part::Refusal(RefusalPart {
            text: "I can't help with that.".into(),
        })];
        response.finish = finish.clone();

        let body = GeminiCodec
            .encode_response(&response, &ctx)
            .expect("encodes");
        assert_eq!(
            body["candidates"][0]["finishReason"],
            json!("SAFETY"),
            "{finish:?}"
        );
        assert_eq!(
            body["candidates"][0]["content"]["parts"],
            json!([{"text": "I can't help with that."}])
        );

        let mut encoder = GeminiCodec.stream_encoder(&ctx);
        let mut wire = Vec::new();
        for event in response_to_events(&response) {
            wire.extend(encoder.encode(&event));
        }
        wire.extend(encoder.finish());
        let reasons: Vec<Value> = wire
            .iter()
            .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok())
            .filter_map(|chunk| chunk["candidates"][0].get("finishReason").cloned())
            .collect();
        assert_eq!(reasons, [json!("SAFETY")], "stream, {finish:?}");
    }
    // An ordinary answer is an ordinary stop.
    let mut response = Response::new("resp_1", "upstream-model");
    response.parts = vec![Part::text("Hello.")];
    let body = GeminiCodec
        .encode_response(&response, &ctx)
        .expect("encodes");
    assert_eq!(body["candidates"][0]["finishReason"], json!("STOP"));
}

/// Without the client's request at hand nothing is renamed.
#[test]
fn names_are_left_alone_when_the_request_is_unknown() {
    let mut response = Response::new("chatcmpl-1", "upstream-model");
    response.parts = vec![Part::tool_call("call_1", "mcp_files_read-file", "{}")];
    response.finish = FinishReason::ToolCalls;
    let body = GeminiCodec
        .encode_response(&response, &ClientCtx::new("gemini-2.5-pro"))
        .expect("encodes");
    assert_eq!(function_call_names(&body), [json!("mcp_files_read-file")]);
}
