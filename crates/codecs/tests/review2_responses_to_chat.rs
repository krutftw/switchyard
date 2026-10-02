//! Review finding (round 2): a Responses client's free-form (`custom`) tools
//! on their way to a Chat Completions upstream.
//!
//! Codex declares `apply_patch` as `{"type":"custom", "format":{grammar}}`
//! and replays its calls as `custom_tool_call` items. The Chat encoder writes
//! both through unchanged in OpenAI's newest Chat spelling
//! (`tools[].type: "custom"`, `tool_calls[].type: "custom"`), which only
//! api.openai.com understands. A Responses request is translated to Chat
//! precisely when the provider does *not* speak Responses, i.e. for the
//! OpenAI-compatible servers (DeepSeek, Kimi, GLM, vLLM, Ollama, …), whose
//! `tools[].type` is `function` and nothing else: they answer 400, a request
//! fault that no failover repairs, on every turn of such a client.
//!
//! The mapping table is explicit (notes 08 §1.3, column "Chat"): a `custom`
//! tool becomes a *function tool* with the schema
//! `{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}`
//! (the same stand-in the Anthropic and the Gemini encoder already use);
//! §5.2: a `custom_tool_call` item becomes a function `tool_calls` entry
//! whose `arguments` is the JSON text `{"input":"<input string>"}`; §5.1:
//! `tool_choice` of type `custom` becomes
//! `{"type":"function","function":{"name":…}}`.
//!
//! The way back already works and is pinned by the second test, which passes:
//! a function call for a tool the client declared as custom is handed to the
//! client as a `custom_tool_call` with the raw input.
//!
//! The third test is the same seam for top-level fields: `store`,
//! `prompt_cache_key` and `service_tier` of a Responses client are copied
//! into the Chat body. Notes 08 §5.1 lists what a Responses request becomes
//! on a Chat upstream and ends with "everything else | dropped". The fields
//! exist on api.openai.com only; servers that validate their request schema
//! (Mistral answers 422 `extra_forbidden`, "Extra inputs are not permitted")
//! refuse the request, and Codex sends `store: false` and a
//! `prompt_cache_key` with every turn. `Quirks` exists for exactly this kind
//! of difference (`stream_usage`, `max_tokens_field`), but has no switch for
//! these.

mod support;

use serde_json::{Value, json};
use support::harness::{
    CHAT, RESPONSES, client_ctx, decode_stream, encode_stream, known_caps, over_the_wire,
    parse_sse, translate_request, validate_request,
};
use switchyard_codecs::codec;
use switchyard_core::UpstreamCtx;

const PATCH: &str = "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch";

/// A Codex turn: the free-form `apply_patch` tool next to an ordinary
/// function, one finished call of each in the history.
fn codex_request(tool_choice: Value) -> Value {
    json!({
        "model": "gpt-5.5",
        "instructions": "You are Codex.",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Fix the typo."}]},
            {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{\"command\":[\"cat\",\"a.txt\"]}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "old"},
            {"type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "input": PATCH},
            {"type": "custom_tool_call_output", "call_id": "call_2", "output": "Done!"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Now do b.txt."}]}
        ],
        "tools": [
            {"type": "function", "name": "shell", "description": "Runs a shell command", "strict": false,
             "parameters": {"type": "object", "properties": {"command": {"type": "array", "items": {"type": "string"}}}, "required": ["command"]}},
            {"type": "custom", "name": "apply_patch", "description": "Edit files with a patch.",
             "format": {"type": "grammar", "syntax": "lark", "definition": "start: /(.|\\n)+/"}}
        ],
        "tool_choice": tool_choice,
        "stream": true,
        "store": false
    })
}

#[test]
fn a_responses_clients_custom_tool_reaches_a_chat_upstream_as_a_function() {
    let mut failures: Vec<String> = Vec::new();
    let caps = known_caps(CHAT);
    for (label, ctx) in [
        ("known model", caps.ctx()),
        ("unknown model", UpstreamCtx::default()),
    ] {
        for tool_choice in [
            json!("auto"),
            json!({"type": "custom", "name": "apply_patch"}),
        ] {
            let body =
                translate_request(RESPONSES, CHAT, &codex_request(tool_choice.clone()), &ctx)
                    .unwrap_or_else(|error| panic!("the request does not translate: {error}"));
            if let Err(violations) = validate_request(CHAT, &body) {
                failures.push(format!(
                    "{label}: the matrix's own Chat validator refuses the body: {violations:?}"
                ));
            }

            // Declarations: both tools are function tools.
            let tools = body["tools"].as_array().expect("tools");
            assert_eq!(
                tools.len(),
                2,
                "{label}: both tools are declared: {tools:?}"
            );
            for tool in tools {
                if tool["type"] != "function" {
                    failures.push(format!(
                        "{label}: tool declared with type {} (only `function` exists on Chat-compatible servers): {tool}",
                        tool["type"]
                    ));
                }
            }
            let patch_tool = tools
                .iter()
                .find(|tool| tool["function"]["name"] == "apply_patch");
            match patch_tool {
                Some(tool) => {
                    let parameters = &tool["function"]["parameters"];
                    if parameters["type"] != "object"
                        || parameters["properties"]["input"]["type"] != "string"
                        || parameters["required"] != json!(["input"])
                    {
                        failures.push(format!(
                            "{label}: the stand-in function does not take one string `input`: {parameters}"
                        ));
                    }
                }
                None => failures.push(format!(
                    "{label}: `apply_patch` is not declared as a function: {tools:?}"
                )),
            }

            // History: the custom call is a function call whose arguments
            // wrap the raw input.
            let calls: Vec<&Value> = body["messages"]
                .as_array()
                .expect("messages")
                .iter()
                .filter_map(|message| message.get("tool_calls").and_then(Value::as_array))
                .flatten()
                .collect();
            let patch_call = calls
                .iter()
                .find(|call| call["id"] == "call_2")
                .unwrap_or_else(|| panic!("{label}: the apply_patch call is missing: {calls:?}"));
            if patch_call["type"] != "function" {
                failures.push(format!(
                    "{label}: the replayed call has type {}: {patch_call}",
                    patch_call["type"]
                ));
            } else {
                let arguments: Value = serde_json::from_str(
                    patch_call["function"]["arguments"].as_str().unwrap_or(""),
                )
                .unwrap_or(Value::Null);
                if arguments != json!({"input": PATCH}) {
                    failures.push(format!(
                        "{label}: the replayed call's arguments are not {{\"input\": <patch>}}: {patch_call}"
                    ));
                }
            }
            // Its output is an ordinary tool message either way.
            assert!(
                body["messages"]
                    .as_array()
                    .expect("messages")
                    .iter()
                    .any(|m| m["role"] == "tool"
                        && m["tool_call_id"] == "call_2"
                        && m["content"] == "Done!"),
                "{label}: the tool output is missing: {}",
                body["messages"]
            );

            // A forced custom tool is a forced function.
            if tool_choice.is_object()
                && body["tool_choice"]
                    != json!({"type": "function", "function": {"name": "apply_patch"}})
            {
                failures.push(format!(
                    "{label}: the forced custom tool is written as {}",
                    body["tool_choice"]
                ));
            }
        }
    }
    failures.sort();
    failures.dedup();
    assert!(
        failures.is_empty(),
        "a Chat-compatible upstream is sent OpenAI-only custom tools:\n{}",
        failures.join("\n")
    );
}

/// The answer side of the same loop, which works today: the Chat upstream
/// calls the stand-in function and the Responses client gets the
/// `custom_tool_call` it declared, with the raw input (complete body and
/// stream). A fix of the request side must keep this.
#[test]
fn the_function_call_of_the_stand_in_comes_back_as_a_custom_tool_call() {
    let request = codex_request(json!("auto"));
    let ctx = client_ctx(RESPONSES, &request);
    let arguments = json!({"input": PATCH}).to_string();

    let answer = json!({
        "id": "chatcmpl-ct1", "object": "chat.completion", "created": 1, "model": "deepseek-chat",
        "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
            "role": "assistant", "content": null,
            "tool_calls": [{"id": "call_9", "type": "function",
                            "function": {"name": "apply_patch", "arguments": arguments}}]
        }}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
    });
    let decoded = codec(CHAT).decode_response(&answer).expect("decodes");
    let body = codec(RESPONSES)
        .encode_response(&decoded, &ctx)
        .expect("encodes");
    let item = &body["output"][0];
    assert_eq!(item["type"], "custom_tool_call", "{item}");
    assert_eq!(item["name"], "apply_patch", "{item}");
    assert_eq!(item["input"], PATCH, "{item}");

    let escaped = serde_json::to_string(&arguments).expect("a JSON string");
    let transcript = format!(
        "data: {{\"id\":\"chatcmpl-ct1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"deepseek-chat\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"tool_calls\":[{{\"index\":0,\"id\":\"call_9\",\"type\":\"function\",\"function\":{{\"name\":\"apply_patch\",\"arguments\":\"\"}}}}]}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"id\":\"chatcmpl-ct1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"deepseek-chat\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":{escaped}}}}}]}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"id\":\"chatcmpl-ct1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"deepseek-chat\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: [DONE]\n\n"
    );
    let events = decode_stream(CHAT, &parse_sse(&transcript));
    let wire = over_the_wire(&encode_stream(RESPONSES, &ctx, &events));
    let done = wire
        .iter()
        .map(|event| serde_json::from_str::<Value>(&event.data).expect("json"))
        .find(|event| event["type"] == "response.output_item.done")
        .expect("an output_item.done event");
    assert_eq!(done["item"]["type"], "custom_tool_call", "{done}");
    assert_eq!(done["item"]["input"], PATCH, "{done}");
}

/// Notes 08 §5.1, Responses -> Chat: after `text.format`,
/// `max_output_tokens`, `instructions`, `input`, tools,
/// `parallel_tool_calls`, `tool_choice` and `reasoning.effort` the table
/// says "everything else | dropped".
#[test]
fn openai_only_request_fields_of_a_responses_client_do_not_reach_a_chat_upstream() {
    let mut request = codex_request(json!("auto"));
    request["prompt_cache_key"] = json!("0199a1b2-codex-session");
    request["service_tier"] = json!("priority");
    let body = translate_request(RESPONSES, CHAT, &request, &UpstreamCtx::default())
        .expect("the request translates");
    let leaked: Vec<String> = ["store", "prompt_cache_key", "service_tier"]
        .iter()
        .filter(|key| body.get(**key).is_some())
        .map(|key| format!("`{key}`: {}", body[*key]))
        .collect();
    assert!(
        leaked.is_empty(),
        "fields that only api.openai.com knows are sent to a Chat-compatible upstream: {}",
        leaked.join(", ")
    );
}
