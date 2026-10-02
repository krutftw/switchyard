//! Regression tests for request decoding defects found in review:
//! stringified image arrays in tool outputs (R7) and namespaced tools named
//! by their local name (R8). The `review_*` tests are the reviewer's
//! originals.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::ir::{MediaPart, Part, Request, ToolChoice};
use switchyard_core::{Codec, RequestPath};

fn decode(body: Value) -> Request {
    ResponsesCodec
        .decode_request(&body, &RequestPath::default())
        .expect("decodes")
}

/// Notes 08 §5.2 "Tool output content" and §10 ("Stringified array with an
/// image -> content-part array"): a `function_call_output.output` that is a
/// *string* holding a JSON array of content parts with at least one valid
/// image is a structured result, not text. Decoding it as text hands the
/// model a base64 blob as prose (and bills it as text tokens) instead of an
/// image.
#[test]
fn review_stringified_image_array_tool_output_is_decoded_as_parts() {
    let stringified = json!([
        {"type": "input_text", "text": "screenshot taken"},
        {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgo="}
    ])
    .to_string();
    let request = decode(json!({
        "model": "gpt-5",
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": "screenshot", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": stringified}
        ]
    }));
    let result = request.messages[1]
        .tool_results()
        .next()
        .expect("tool result");
    assert_eq!(
        result.content,
        vec![
            Part::text("screenshot taken"),
            Part::Image(MediaPart::base64("image/png", "iVBORw0KGgo=")),
        ]
    );
}

/// Strings that merely look like JSON must stay text (same notes: "a
/// stringified text-only array, a JSON object … stay strings unchanged").
/// Guards the fix for the test above against over-eager parsing; passes
/// today.
#[test]
fn review_stringified_text_only_array_tool_output_stays_text() {
    let stringified = json!([{"type": "input_text", "text": "done"}]).to_string();
    let request = decode(json!({
        "model": "gpt-5",
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": stringified.clone()}
        ]
    }));
    let result = request.messages[1].tool_results().next().unwrap();
    assert_eq!(result.content, vec![Part::text(stringified)]);
}

/// The decoder flattens `namespace` tools to `<ns>__<child>` (an extension
/// the crate chose to keep). Having done so it must resolve the other places
/// that name the same tool the way the notes describe (08 §4.4 "Resolving a
/// name seen in history or `tool_choice`", §10 "tool_choice: … namespace
/// child by local name", "Local-name recovery"): a unique local name refers
/// to the declared child. Otherwise `tool_choice` forces a tool that is not
/// among `tools` (a 400 on Anthropic and on Chat upstreams) and a replayed
/// call names a tool the upstream never saw.
#[test]
fn review_namespaced_tool_is_resolved_by_its_local_name() {
    let tools = json!([
        {"type": "namespace", "name": "mcp__github", "description": "GitHub", "tools": [
            {"type": "function", "name": "get_me", "description": "who am I", "parameters": {"type": "object", "properties": {}}}
        ]}
    ]);

    let request = decode(json!({
        "model": "gpt-5",
        "input": "who am I?",
        "tools": tools,
        "tool_choice": {"type": "function", "name": "get_me"}
    }));
    assert_eq!(request.tools[0].name(), Some("mcp__github__get_me"));
    assert_eq!(
        request.tool_choice,
        Some(ToolChoice::Tool {
            name: "mcp__github__get_me".into()
        }),
        "tool_choice must name the tool as it was flattened into `tools`"
    );

    let request = decode(json!({
        "model": "gpt-5",
        "input": [
            {"type": "message", "role": "user", "content": "who am I?"},
            {"type": "function_call", "call_id": "call_1", "name": "get_me", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "octocat"}
        ],
        "tools": tools
    }));
    let call = request.messages[1].tool_calls().next().expect("tool call");
    assert_eq!(
        call.name, "mcp__github__get_me",
        "a replayed call must use the same flat name as the declaration"
    );
}

fn tool_output(output: Value) -> Vec<Part> {
    let request = decode(json!({
        "model": "gpt-5",
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": output}
        ]
    }));
    request.messages[1]
        .tool_results()
        .next()
        .expect("tool result")
        .content
        .clone()
}

/// The stringified form is recognised for every spelling of an image part,
/// and entries that are neither text nor image survive as text.
#[test]
fn stringified_tool_output_accepts_chat_style_and_unknown_parts() {
    let stringified = json!([
        {"type": "text", "text": "before"},
        {"type": "image_url", "image_url": {"url": "https://example.com/shot.png", "detail": "low"}},
        {"type": "resource", "uri": "file:///tmp/x"},
        {"type": "output_text", "text": ""},
        {"type": "input_image", "file_id": "file-abc"}
    ])
    .to_string();
    let content = tool_output(json!(format!("  {stringified}\n")));
    let mut url_image = MediaPart::url("https://example.com/shot.png");
    url_image.detail = Some("low".into());
    assert_eq!(content.len(), 4, "{content:?}");
    assert_eq!(content[0], Part::text("before"));
    assert_eq!(content[1], Part::Image(url_image));
    assert_eq!(
        content[2],
        Part::text(r#"{"type":"resource","uri":"file:///tmp/x"}"#)
    );
    assert!(matches!(&content[3], Part::Image(_)));
}

/// Everything short of a well-formed part list with an image stays the
/// string the tool returned, byte for byte (notes 08 §5.2 / §10).
#[test]
fn stringified_tool_output_that_is_not_a_clean_image_list_stays_text() {
    let image = json!({"type": "input_image", "image_url": "data:image/png;base64,AAAA"});
    let untouched = [
        // plain text and JSON that is not an array
        "all done".to_string(),
        json!({"type": "input_image", "image_url": "data:image/png;base64,AAAA"}).to_string(),
        json!({"image_url": "https://example.com/a.png"}).to_string(),
        "[]".to_string(),
        "[1, 2, 3]".to_string(),
        // an image part without a usable URL
        json!([{"type": "input_image"}]).to_string(),
        json!([{"type": "input_image", "image_url": ""}]).to_string(),
        json!([{"type": "input_image", "image_url": 42}]).to_string(),
        json!([{"type": "image_url", "image_url": {"url": null}}]).to_string(),
        // malformed neighbours of a good image
        json!([image, {"type": "input_text", "text": 7}]).to_string(),
        json!([image, {"type": "input_text"}]).to_string(),
        json!([{"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": 3}])
            .to_string(),
        json!([{"type": "image_url", "image_url": {"url": "https://example.com/a.png", "detail": false}}])
            .to_string(),
        // truncated, or with trailing garbage
        format!("[{image}"),
        format!("[{image}] and then some"),
    ];
    for text in untouched {
        assert_eq!(
            tool_output(json!(text.clone())),
            vec![Part::text(text.clone())],
            "{text}"
        );
    }
}

/// The same goes for custom tool outputs and for the structured (array)
/// form, which was already understood.
#[test]
fn image_tool_output_is_understood_in_every_form() {
    let parts = json!([
        {"type": "input_text", "text": "shot"},
        {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
    ]);
    let expected = vec![
        Part::text("shot"),
        Part::Image(MediaPart::base64("image/png", "AAAA")),
    ];
    assert_eq!(tool_output(parts.clone()), expected);
    assert_eq!(tool_output(json!(parts.to_string())), expected);

    let request = decode(json!({
        "model": "gpt-5",
        "input": [
            {"type": "custom_tool_call", "call_id": "call_1", "name": "shell", "input": "screencap"},
            {"type": "custom_tool_call_output", "call_id": "call_1", "output": parts.to_string()}
        ]
    }));
    let result = request.messages[1].tool_results().next().unwrap();
    assert_eq!(result.content, expected);
}

fn namespaced_tools() -> Value {
    json!([
        {"type": "namespace", "name": "mcp__github", "tools": [
            {"type": "function", "name": "get_me", "parameters": {"type": "object", "properties": {}}},
            {"type": "function", "name": "search", "parameters": {"type": "object", "properties": {}}}
        ]},
        {"type": "namespace", "name": "mcp__jira", "tools": [
            {"type": "function", "name": "search", "parameters": {"type": "object", "properties": {}}},
            {"type": "custom", "name": "exec"}
        ]},
        {"type": "function", "name": "exec", "parameters": {"type": "object", "properties": {}}}
    ])
}

fn choice_for(tool_choice: Value) -> Option<ToolChoice> {
    decode(json!({
        "model": "gpt-5", "input": "go", "tools": namespaced_tools(), "tool_choice": tool_choice
    }))
    .tool_choice
}

fn forced(name: &str) -> Option<ToolChoice> {
    Some(ToolChoice::Tool { name: name.into() })
}

/// Resolution rules of notes 08 §4.4: the exact flat name, else a local name
/// only one declaration carries; an explicit namespace always wins; nothing
/// is guessed.
#[test]
fn tool_choice_names_resolve_like_the_declarations() {
    // Unique local name, flat or nested spelling.
    assert_eq!(
        choice_for(json!({"type": "function", "name": "get_me"})),
        forced("mcp__github__get_me")
    );
    assert_eq!(
        choice_for(json!({"type": "function", "function": {"name": "get_me"}})),
        forced("mcp__github__get_me")
    );
    // Already flat, or spelled with its namespace.
    assert_eq!(
        choice_for(json!({"type": "function", "name": "mcp__github__get_me"})),
        forced("mcp__github__get_me")
    );
    assert_eq!(
        choice_for(json!({"type": "function", "name": "search", "namespace": "mcp__jira"})),
        forced("mcp__jira__search")
    );
    // Two namespaces declare `search`: not guessed.
    assert_eq!(
        choice_for(json!({"type": "function", "name": "search"})),
        forced("search")
    );
    // A flat `exec` exists: the exact name beats the namespace child.
    assert_eq!(
        choice_for(json!({"type": "function", "name": "exec"})),
        forced("exec")
    );
    assert_eq!(
        choice_for(json!({"type": "custom", "name": "exec", "namespace": "mcp__jira"})),
        forced("mcp__jira__exec")
    );
    // Unknown names pass through untouched.
    assert_eq!(
        choice_for(json!({"type": "function", "name": "nope"})),
        forced("nope")
    );
    assert_eq!(choice_for(json!("required")), Some(ToolChoice::Required));
}

/// Replayed calls follow the same rules, whether the tools were declared at
/// the top level or through an `additional_tools` item, and tool results keep
/// pairing with their calls.
#[test]
fn replayed_call_names_resolve_like_the_declarations() {
    let request = decode(json!({
        "model": "gpt-5",
        "tools": namespaced_tools(),
        "input": [
            {"type": "additional_tools", "tools": [
                {"type": "namespace", "name": "fs", "tools": [{"type": "custom", "name": "patch"}]}
            ]},
            {"type": "message", "role": "user", "content": "go"},
            {"type": "function_call", "call_id": "c1", "name": "get_me", "arguments": "{}"},
            {"type": "function_call", "call_id": "c2", "name": "search", "arguments": "{}"},
            {"type": "function_call", "call_id": "c3", "name": "search", "namespace": "mcp__jira", "arguments": "{}"},
            {"type": "function_call", "call_id": "c4", "name": "exec", "arguments": "{}"},
            {"type": "custom_tool_call", "call_id": "c5", "name": "patch", "input": "*** Begin"},
            {"type": "function_call", "call_id": "c6", "name": "undeclared", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "octocat"},
            {"type": "function_call_output", "name": "search", "output": "first id-less output"},
            {"type": "custom_tool_call_output", "call_id": "c5", "output": "ok"}
        ]
    }));
    let names: Vec<(&str, &str)> = request.messages[1]
        .tool_calls()
        .map(|call| (call.id.as_str(), call.name.as_str()))
        .collect();
    assert_eq!(
        names,
        vec![
            ("c1", "mcp__github__get_me"),
            ("c2", "search"),
            ("c3", "mcp__jira__search"),
            ("c4", "exec"),
            ("c5", "fs__patch"),
            ("c6", "undeclared"),
        ]
    );
    assert!(
        request
            .tools
            .iter()
            .any(|tool| tool.name() == Some("fs__patch"))
    );
    // The id-less output still found its call by the name the client used.
    let results: Vec<&str> = request.messages[2]
        .tool_results()
        .map(|result| result.call_id.as_str())
        .collect();
    assert_eq!(results, vec!["c1", "c2", "c5"]);
}
