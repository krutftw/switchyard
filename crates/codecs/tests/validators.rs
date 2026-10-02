//! The validators validate: literal payloads a vendor is known to accept must
//! pass, and literal payloads with one specific defect must be rejected with
//! a message naming it. Without this the matrix could be green because its
//! oracle is blind.

mod support;

use serde_json::{Value, json};
use support::harness::parse_sse;
use support::scenarios;
use support::validators::*;
use switchyard_core::Protocol;

/// Asserts that `report` failed and that one violation mentions `needle`.
#[track_caller]
fn rejects(report: Report, needle: &str) {
    match report {
        Ok(()) => panic!("expected a violation mentioning `{needle}`, but the payload passed"),
        Err(violations) => assert!(
            violations.iter().any(|v| v.contains(needle)),
            "no violation mentions `{needle}`: {violations:#?}"
        ),
    }
}

#[track_caller]
fn accepts(report: Report) {
    if let Err(violations) = report {
        panic!("expected the payload to pass: {violations:#?}");
    }
}

/// Returns `base` with the value at `pointer` replaced (or inserted).
fn with(mut base: Value, pointer: &str, value: Value) -> Value {
    let (parent, key) = pointer.rsplit_once('/').expect("a JSON pointer");
    let parent = if parent.is_empty() {
        &mut base
    } else {
        base.pointer_mut(parent).expect("the parent exists")
    };
    match parent {
        Value::Object(map) => {
            map.insert(key.to_string(), value);
        }
        Value::Array(list) => {
            let index: usize = key.parse().expect("an array index");
            if index < list.len() {
                list[index] = value;
            } else {
                list.push(value);
            }
        }
        other => panic!("cannot index into {other}"),
    }
    base
}

fn without(mut base: Value, pointer: &str) -> Value {
    let (parent, key) = pointer.rsplit_once('/').expect("a JSON pointer");
    let parent = if parent.is_empty() {
        &mut base
    } else {
        base.pointer_mut(parent).expect("the parent exists")
    };
    match parent {
        Value::Object(map) => {
            map.shift_remove(key);
        }
        Value::Array(list) => {
            list.remove(key.parse().expect("an array index"));
        }
        other => panic!("cannot index into {other}"),
    }
    base
}

// ---------------------------------------------------------------------------
// Chat
// ---------------------------------------------------------------------------

fn chat_tool_loop() -> Value {
    json!({
        "model": "gpt-5.5",
        "messages": [
            {"role": "system", "content": "You are a weather bot."},
            {"role": "user", "content": [
                {"type": "text", "text": "Weather in Paris?"},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png", "detail": "low"}}
            ]},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"location\":\"Paris\"}"}},
                {"id": "call_2", "type": "function", "function": {"name": "get_weather", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_2", "content": "sunny"},
            {"role": "tool", "tool_call_id": "call_1", "content": "cloudy"},
            {"role": "user", "content": "Thanks."}
        ],
        "tools": [{"type": "function", "function": {
            "name": "get_weather",
            "parameters": {"type": "object", "properties": {"location": {"type": "string"}}}
        }}],
        "tool_choice": "auto",
        "max_completion_tokens": 100,
        "temperature": 0.5,
        "stream": true,
        "stream_options": {"include_usage": true}
    })
}

#[test]
fn chat_request_known_good() {
    accepts(validate_chat_request(&chat_tool_loop()));
    accepts(validate_chat_request(&json!({
        "model": "gpt-5.5",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "xhigh",
        "response_format": {"type": "json_schema", "json_schema": {"name": "out", "schema": {"type": "object"}}},
        "stop": ["a", "b"]
    })));
}

#[test]
fn chat_request_known_bad() {
    let good = chat_tool_loop();
    rejects(
        validate_chat_request(&without(good.clone(), "/model")),
        "model",
    );
    rejects(
        validate_chat_request(&with(good.clone(), "/messages", json!([]))),
        "must not be empty",
    );
    // A tool message that answers nothing.
    rejects(
        validate_chat_request(&with(
            good.clone(),
            "/messages/3/tool_call_id",
            json!("call_9"),
        )),
        "answers no pending tool call",
    );
    // A call nobody answers.
    rejects(
        validate_chat_request(&without(good.clone(), "/messages/4")),
        "not answered",
    );
    // A tool message that does not follow its assistant message.
    let mut moved = good.clone();
    let tool = moved["messages"].as_array_mut().unwrap().remove(4);
    moved["messages"].as_array_mut().unwrap().push(tool);
    rejects(validate_chat_request(&moved), "not answered");
    rejects(
        validate_chat_request(&with(
            good.clone(),
            "/messages/2/tool_calls/0/function/arguments",
            json!("{\"location\":"),
        )),
        "not a JSON object",
    );
    rejects(
        validate_chat_request(&with(
            good.clone(),
            "/messages/2/tool_calls/0/function/arguments",
            json!({"location": "Paris"}),
        )),
        "must be a string",
    );
    rejects(
        validate_chat_request(&with(
            good.clone(),
            "/tools/0/function/name",
            json!("get.weather"),
        )),
        "does not match",
    );
    rejects(
        validate_chat_request(&with(
            good.clone(),
            "/tools/0/function/name",
            json!("x".repeat(65)),
        )),
        "does not match",
    );
    rejects(
        validate_chat_request(&with(
            good.clone(),
            "/messages/2/tool_calls/0/id",
            json!("c".repeat(41)),
        )),
        "maximum is 40",
    );
    rejects(
        validate_chat_request(&with(
            good.clone(),
            "/tools/0/function/parameters",
            json!({"type": "object"}),
        )),
        "missing `properties`",
    );
    rejects(
        validate_chat_request(&without(good.clone(), "/tools")),
        "only allowed when tools",
    );
    rejects(
        validate_chat_request(&with(
            good.clone(),
            "/tool_choice",
            json!({"type": "function", "function": {"name": "other"}}),
        )),
        "not a declared tool",
    );
    rejects(
        validate_chat_request(&with(good.clone(), "/stream", json!(false))),
        "only allowed when stream is true",
    );
    rejects(
        validate_chat_request(&with(good.clone(), "/max_tokens", json!(5))),
        "both set",
    );
    rejects(
        validate_chat_request(&with(good.clone(), "/temperature", json!(2.5))),
        "outside",
    );
    rejects(
        validate_chat_request(&with(good.clone(), "/metadata", json!({"a": "b"}))),
        "store is enabled",
    );
    rejects(
        validate_chat_request(&with(good.clone(), "/system", json!("x"))),
        "unknown field `system`",
    );
    rejects(
        validate_chat_request(&with(good.clone(), "/reasoning_effort", json!("ultra"))),
        "invalid value",
    );
    rejects(
        validate_chat_request(&with(
            good,
            "/messages/1/content/1",
            json!({"type": "image", "source": {}}),
        )),
        "unknown content part type",
    );
}

/// The output-format rules of the OpenAI API.
#[test]
fn chat_output_format_rules() {
    let base = json!({
        "model": "gpt-5.5",
        "messages": [{"role": "user", "content": "List two recipes as JSON."}]
    });
    let schema_format = |schema: Value| json!({"type": "json_schema", "json_schema": {"name": "out", "schema": schema}});
    accepts(validate_chat_request(&with(
        base.clone(),
        "/response_format",
        json!({"type": "json_object"}),
    )));
    accepts(validate_chat_request(&with(
        base.clone(),
        "/response_format",
        schema_format(json!({"type": "object", "properties": {}})),
    )));
    // "schema must be a JSON Schema of 'type: \"object\"', got 'type: \"array\"'."
    rejects(
        validate_chat_request(&with(
            base.clone(),
            "/response_format",
            schema_format(json!({"type": "array", "items": {"type": "string"}})),
        )),
        "schema must be of type `object`",
    );
    rejects(
        validate_chat_request(&with(
            base.clone(),
            "/response_format",
            schema_format(json!({"properties": {}})),
        )),
        "schema must be of type `object`",
    );
    // "'messages' must contain the word 'json' in some form."
    let silent = with(base, "/messages/0/content", json!("List two recipes."));
    rejects(
        validate_chat_request(&with(
            silent,
            "/response_format",
            json!({"type": "json_object"}),
        )),
        "needs the word `json`",
    );
}

/// The profile of an OpenAI-compatible server: `function` tools only and
/// none of OpenAI's platform fields.
#[test]
fn chat_compatible_request_known_good_and_bad() {
    let good = chat_tool_loop();
    accepts(validate_chat_compatible_request(&good));
    // Whatever the plain validator rejects is rejected here too.
    rejects(
        validate_chat_compatible_request(&without(good.clone(), "/model")),
        "model",
    );
    for (pointer, value) in [
        ("/store", json!(false)),
        ("/prompt_cache_key", json!("session-1")),
        ("/service_tier", json!("priority")),
        ("/safety_identifier", json!("user-1")),
    ] {
        let body = with(good.clone(), pointer, value);
        accepts(validate_chat_request(&body));
        rejects(
            validate_chat_compatible_request(&body),
            "OpenAI's own platform only",
        );
    }
    let custom_tool = with(
        good.clone(),
        "/tools/1",
        json!({"type": "custom", "custom": {"name": "apply_patch"}}),
    );
    accepts(validate_chat_request(&custom_tool));
    rejects(
        validate_chat_compatible_request(&custom_tool),
        "compatible servers know `function` only",
    );
    rejects(
        validate_chat_compatible_request(&with(
            custom_tool,
            "/tool_choice",
            json!({"type": "custom", "custom": {"name": "apply_patch"}}),
        )),
        "tool_choice: type",
    );
}

fn chat_completion() -> Value {
    json!({
        "id": "chatcmpl-1", "object": "chat.completion", "created": 1759400000, "model": "gpt-5.5",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hi.", "refusal": null},
                     "logprobs": null, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
                  "prompt_tokens_details": {"cached_tokens": 4},
                  "completion_tokens_details": {"reasoning_tokens": 2}}
    })
}

#[test]
fn chat_response_known_good_and_bad() {
    let good = chat_completion();
    accepts(validate_chat_response(&good));
    rejects(
        validate_chat_response(&with(
            good.clone(),
            "/choices/0/finish_reason",
            json!("end_turn"),
        )),
        "invalid finish_reason",
    );
    rejects(
        validate_chat_response(&with(good.clone(), "/usage/total_tokens", json!(99))),
        "total_tokens",
    );
    rejects(
        validate_chat_response(&with(
            good.clone(),
            "/usage/prompt_tokens_details/cached_tokens",
            json!(11),
        )),
        "exceed prompt_tokens",
    );
    rejects(
        validate_chat_response(&with(
            good.clone(),
            "/choices/0/message/tool_calls",
            json!([{"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{}"}}]),
        )),
        "finish_reason `stop`",
    );
    rejects(
        validate_chat_response(&with(
            good.clone(),
            "/choices/0/finish_reason",
            json!("tool_calls"),
        )),
        "there are none",
    );
    rejects(
        validate_chat_response(&with(good, "/object", json!("chat.completion.chunk"))),
        "chat.completion",
    );
}

#[test]
fn chat_stream_known_good_and_bad() {
    for fixture in scenarios::responses(Protocol::OpenaiChat) {
        if fixture.end == scenarios::StreamEnd::Truncated {
            continue;
        }
        let events = parse_sse(fixture.sse.unwrap());
        accepts(validate_chat_stream(&events));
    }
    let text = scenarios::responses(Protocol::OpenaiChat)
        .into_iter()
        .find(|f| f.name == "text")
        .unwrap();
    let events = parse_sse(text.sse.unwrap());
    // No terminator.
    rejects(validate_chat_stream(&events[..events.len() - 1]), "[DONE]");
    // No finish chunk.
    let mut missing = events.clone();
    missing.remove(3);
    rejects(
        validate_chat_stream(&missing),
        "usage chunk before the finish",
    );
    // Content before the role chunk.
    rejects(validate_chat_stream(&events[1..]), "before the role chunk");
    // A named event.
    let mut named = events.clone();
    named[1].event = Some("delta".into());
    rejects(validate_chat_stream(&named), "data-only");
    // Tool-call fragments that do not add up to JSON.
    let tool = scenarios::responses(Protocol::OpenaiChat)
        .into_iter()
        .find(|f| f.name == "tool_call")
        .unwrap();
    let mut events = parse_sse(tool.sse.unwrap());
    events.remove(2);
    rejects(validate_chat_stream(&events), "not a JSON object");
    // The truncated transcript is not a valid stream.
    let cut = scenarios::responses(Protocol::OpenaiChat)
        .into_iter()
        .find(|f| f.name == "truncated")
        .unwrap();
    rejects(validate_chat_stream(&parse_sse(cut.sse.unwrap())), "[DONE]");
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

fn responses_tool_loop() -> Value {
    json!({
        "model": "gpt-5.5",
        "instructions": "You are a weather bot.",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Weather in Paris?"}]},
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Call it."}],
             "encrypted_content": "gAAAAAB"},
            {"type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"location\":\"Paris\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "cloudy"},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "Thanks."},
                {"type": "input_image", "image_url": "https://example.com/a.png"}
            ]}
        ],
        "tools": [{"type": "function", "name": "get_weather",
                   "parameters": {"type": "object", "properties": {"location": {"type": "string"}}}, "strict": false}],
        "tool_choice": {"type": "function", "name": "get_weather"},
        "reasoning": {"effort": "high", "summary": "auto"},
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "max_output_tokens": 100,
        "stream": true
    })
}

#[test]
fn responses_request_known_good() {
    accepts(validate_responses_request(&responses_tool_loop()));
    accepts(validate_responses_request(
        &json!({"model": "gpt-5.5", "input": "hi"}),
    ));
}

#[test]
fn responses_request_known_bad() {
    let good = responses_tool_loop();
    rejects(
        validate_responses_request(&with(good.clone(), "/input/3/call_id", json!("call_9"))),
        "matches no function_call",
    );
    rejects(
        validate_responses_request(&without(good.clone(), "/input/3")),
        "no tool output found",
    );
    // A reasoning item that nothing follows.
    let mut dangling = good.clone();
    let reasoning = dangling["input"].as_array_mut().unwrap().remove(1);
    dangling["input"].as_array_mut().unwrap().push(reasoning);
    rejects(
        validate_responses_request(&dangling),
        "not followed by output",
    );
    rejects(
        validate_responses_request(&without(good.clone(), "/input/1/encrypted_content")),
        "needs stored state",
    );
    rejects(
        validate_responses_request(&with(
            good.clone(),
            "/input/1/encrypted_content",
            json!("sy1.a.EqQBCk"),
        )),
        "gateway-wrapped",
    );
    rejects(
        validate_responses_request(&with(good.clone(), "/input/1/id", json!("msg_1"))),
        "invalid reasoning item id",
    );
    rejects(
        validate_responses_request(&with(good.clone(), "/input/2/arguments", json!("nope"))),
        "not a JSON object",
    );
    // Chat spellings.
    rejects(
        validate_responses_request(&with(
            good.clone(),
            "/tools/0",
            json!({"type": "function", "function": {"name": "get_weather"}}),
        )),
        "unknown field `function`",
    );
    rejects(
        validate_responses_request(&with(
            good.clone(),
            "/tool_choice",
            json!({"type": "function", "function": {"name": "get_weather"}}),
        )),
        "`name` is required",
    );
    rejects(
        validate_responses_request(&with(
            good.clone(),
            "/input/4/content/1",
            json!({"type": "input_image", "image_url": {"url": "https://example.com/a.png"}}),
        )),
        "image_url (a string)",
    );
    rejects(
        validate_responses_request(&with(good.clone(), "/messages", json!([]))),
        "unknown field `messages`",
    );
    rejects(
        validate_responses_request(&with(good.clone(), "/max_output_tokens", json!(8))),
        "below the minimum",
    );
    rejects(
        validate_responses_request(&with(good.clone(), "/reasoning/summary", Value::Null)),
        "must be a string",
    );
    rejects(
        validate_responses_request(&with(good.clone(), "/include", json!(["reasoning"]))),
        "invalid entry",
    );
    rejects(
        validate_responses_request(&with(
            good.clone(),
            "/stream_options",
            json!({"include_usage": true}),
        )),
        "does not exist on Responses",
    );
    rejects(
        validate_responses_request(&with(
            good,
            "/input/0/content/0",
            json!({"type": "output_text", "text": "x"}),
        )),
        "not valid in an input message",
    );
}

/// The output-format and hosted-tool rules of the Responses API.
#[test]
fn responses_output_format_and_hosted_choice_rules() {
    let base = json!({"model": "gpt-5.5", "input": "List two recipes as JSON."});
    let schema_format =
        |schema: Value| json!({"format": {"type": "json_schema", "name": "out", "schema": schema}});
    accepts(validate_responses_request(&with(
        base.clone(),
        "/text",
        json!({"format": {"type": "json_object"}}),
    )));
    accepts(validate_responses_request(&with(
        base.clone(),
        "/text",
        schema_format(json!({"type": "object", "properties": {}})),
    )));
    rejects(
        validate_responses_request(&with(
            base.clone(),
            "/text",
            schema_format(json!({"type": "string", "enum": ["a", "b"]})),
        )),
        "schema must be of type `object`",
    );
    let silent = with(base.clone(), "/input", json!("List two recipes."));
    rejects(
        validate_responses_request(&with(
            silent.clone(),
            "/text",
            json!({"format": {"type": "json_object"}}),
        )),
        "needs the word `json`",
    );
    // Said in the instructions: accepted (the rule's reach is not documented).
    accepts(validate_responses_request(&with(
        with(silent, "/instructions", json!("Answer in JSON.")),
        "/text",
        json!({"format": {"type": "json_object"}}),
    )));

    // Forcing a hosted tool needs the tool.
    let searching = with(base, "/tools", json!([{"type": "web_search"}]));
    accepts(validate_responses_request(&with(
        searching.clone(),
        "/tool_choice",
        json!({"type": "web_search"}),
    )));
    rejects(
        validate_responses_request(&with(
            searching,
            "/tool_choice",
            json!({"type": "code_interpreter"}),
        )),
        "is not among the tools",
    );
}

#[test]
fn responses_response_and_stream_known_good_and_bad() {
    for fixture in scenarios::responses(Protocol::OpenaiResponses) {
        if let Some(body) = &fixture.json {
            accepts(validate_responses_response(body));
        }
        if fixture.end != scenarios::StreamEnd::Truncated {
            accepts(validate_responses_stream(&parse_sse(fixture.sse.unwrap())));
        }
    }
    let fixtures = scenarios::responses(Protocol::OpenaiResponses);
    let text = fixtures.iter().find(|f| f.name == "text").unwrap();
    let body = text.json.clone().unwrap();
    rejects(
        validate_responses_response(&with(body.clone(), "/id", json!("chatcmpl-1"))),
        "resp_",
    );
    rejects(
        validate_responses_response(&with(body.clone(), "/status", json!("incomplete"))),
        "incomplete_details.reason",
    );
    rejects(
        validate_responses_response(&with(body.clone(), "/usage/total_tokens", json!(1))),
        "total_tokens",
    );
    rejects(
        validate_responses_response(&with(body.clone(), "/output_text", json!("x"))),
        "not a wire field",
    );
    rejects(
        validate_responses_response(&with(body, "/output/0/content/0/annotations", Value::Null)),
        "annotations must be an array",
    );

    let events = parse_sse(text.sse.unwrap());
    rejects(
        validate_responses_stream(&events[..events.len() - 1]),
        "no terminal event",
    );
    let mut gap = events.clone();
    gap.remove(4);
    rejects(validate_responses_stream(&gap), "sequence_number");
    rejects(
        validate_responses_stream(&events[1..]),
        "must open with response.created",
    );
    let mut done = events.clone();
    done.push(switchyard_core::SseEvent::data("[DONE]"));
    rejects(validate_responses_stream(&done), "after the terminal event");
    let mut unnamed = events;
    unnamed[2].event = None;
    rejects(validate_responses_stream(&unnamed), "SSE event name");
    let cut = fixtures.iter().find(|f| f.name == "truncated").unwrap();
    rejects(
        validate_responses_stream(&parse_sse(cut.sse.unwrap())),
        "no terminal event",
    );
}

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

fn anthropic_tool_loop() -> Value {
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 16000,
        "system": [{"type": "text", "text": "You are a weather bot.", "cache_control": {"type": "ephemeral"}}],
        "thinking": {"type": "enabled", "budget_tokens": 4096},
        "messages": [
            {"role": "user", "content": "Weather in Paris?"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Call it.", "signature": "EqQBCkYICBgC"},
                {"type": "tool_use", "id": "toolu_01", "name": "get_weather", "input": {"location": "Paris"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01", "content": "cloudy"},
                {"type": "text", "text": "Thanks."}
            ]}
        ],
        "tools": [{"name": "get_weather", "input_schema": {"type": "object", "properties": {}}}],
        "tool_choice": {"type": "auto"}
    })
}

#[test]
fn anthropic_request_known_good() {
    accepts(validate_anthropic_request(&anthropic_tool_loop()));
    accepts(validate_anthropic_request(&json!({
        "model": "claude-opus-5", "max_tokens": 1024,
        "thinking": {"type": "adaptive", "display": "summarized"},
        "output_config": {"effort": "xhigh"},
        "messages": [{"role": "user", "content": "hi"}]
    })));
}

#[test]
fn anthropic_request_known_bad() {
    let good = anthropic_tool_loop();
    rejects(
        validate_anthropic_request(&without(good.clone(), "/max_tokens")),
        "max_tokens",
    );
    rejects(
        validate_anthropic_request(&with(good.clone(), "/thinking/budget_tokens", json!(16000))),
        "less than max_tokens",
    );
    rejects(
        validate_anthropic_request(&with(good.clone(), "/thinking/budget_tokens", json!(512))),
        "below 1024",
    );
    rejects(
        validate_anthropic_request(&with(good.clone(), "/temperature", json!(0.5))),
        "while thinking is active",
    );
    let sampling = with(
        without(good.clone(), "/thinking"),
        "/temperature",
        json!(0.5),
    );
    accepts(validate_anthropic_request(&sampling));
    rejects(
        validate_anthropic_request(&with(sampling, "/top_p", json!(0.9))),
        "cannot both be specified",
    );
    // Unsigned and gateway-wrapped thinking.
    rejects(
        validate_anthropic_request(&with(
            good.clone(),
            "/messages/1/content/0/signature",
            json!(""),
        )),
        "without a signature",
    );
    rejects(
        validate_anthropic_request(&with(
            good.clone(),
            "/messages/1/content/0/signature",
            json!("sy1.g.CiQB"),
        )),
        "gateway-wrapped",
    );
    // Manual thinking with a turn in progress that does not open with it.
    rejects(
        validate_anthropic_request(&without(good.clone(), "/messages/1/content/0")),
        "does not start with a thinking block",
    );
    rejects(
        validate_anthropic_request(&with(good.clone(), "/messages/0/role", json!("assistant"))),
        "first message must be a user message",
    );
    rejects(
        validate_anthropic_request(&with(
            good.clone(),
            "/messages/2/content/0/tool_use_id",
            json!("toolu_02"),
        )),
        "no tool_use in the previous message",
    );
    rejects(
        validate_anthropic_request(&without(good.clone(), "/messages/2/content/0")),
        "no tool_result in the next message",
    );
    // Text ahead of the tool result.
    let mut reordered = good.clone();
    reordered["messages"][2]["content"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    rejects(validate_anthropic_request(&reordered), "must come first");
    rejects(
        validate_anthropic_request(&with(
            good.clone(),
            "/messages/1/content/1/id",
            json!("call.1"),
        )),
        "does not match",
    );
    rejects(
        validate_anthropic_request(&with(
            good.clone(),
            "/messages/2/content/1/text",
            json!("  \n"),
        )),
        "non-whitespace",
    );
    rejects(
        validate_anthropic_request(&with(good.clone(), "/tool_choice", json!({"type": "any"}))),
        "cannot be combined with thinking",
    );
    rejects(
        validate_anthropic_request(&with(good.clone(), "/tools/0/name", json!("get.weather"))),
        "does not match",
    );
    rejects(
        validate_anthropic_request(&with(
            good.clone(),
            "/tools/0/input_schema",
            json!({"anyOf": [{"type": "object"}]}),
        )),
        "input_schema",
    );
    rejects(
        validate_anthropic_request(&with(
            good.clone(),
            "/messages/3",
            json!({"role": "user", "content": "again"}),
        )),
        "two consecutive `user` messages",
    );
    rejects(
        validate_anthropic_request(&with(good, "/reasoning_effort", json!("high"))),
        "unknown field",
    );
}

/// The rules that depend on the Claude generation (notes 15 §5.2).
#[test]
fn anthropic_model_generation_rules() {
    let request = |model: &str| {
        json!({
            "model": model, "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "get_weather", "input_schema": {"type": "object", "properties": {}}}]
        })
    };
    let any = json!({"type": "any"});
    let tool = json!({"type": "tool", "name": "get_weather"});
    let prefill = json!({"role": "assistant", "content": "Here:"});

    // Forced tool use: refused by Opus 5.5, Sonnet 5.5, Fable 5.1, Mythos 5.1.
    for model in ["claude-sonnet-4-5", "claude-opus-4-6", "claude-opus-5-0"] {
        accepts(validate_anthropic_request(&with(
            request(model),
            "/tool_choice",
            any.clone(),
        )));
    }
    for model in [
        "claude-opus-5-5",
        "claude-sonnet-5-5-20260601",
        "claude-fable-5-1",
        "claude-mythos-5-1",
    ] {
        for choice in [&any, &tool] {
            rejects(
                validate_anthropic_request(&with(request(model), "/tool_choice", choice.clone())),
                "not supported for this model",
            );
        }
        accepts(validate_anthropic_request(&with(
            request(model),
            "/tool_choice",
            json!({"type": "auto"}),
        )));
    }

    // Prefill: refused by Claude 4.6 and later.
    for model in ["claude-sonnet-4-5", "claude-opus-4-1-20250805"] {
        accepts(validate_anthropic_request(&with(
            request(model),
            "/messages/1",
            prefill.clone(),
        )));
    }
    for model in ["claude-opus-4-6", "claude-sonnet-4-6", "claude-sonnet-5-5"] {
        rejects(
            validate_anthropic_request(&with(request(model), "/messages/1", prefill.clone())),
            "does not support assistant message prefill",
        );
    }
}

#[test]
fn anthropic_response_and_stream_known_good_and_bad() {
    let fixtures = scenarios::responses(Protocol::Anthropic);
    for fixture in &fixtures {
        if let Some(body) = &fixture.json {
            accepts(validate_anthropic_response(body));
        }
        if fixture.end != scenarios::StreamEnd::Truncated {
            accepts(validate_anthropic_stream(&parse_sse(fixture.sse.unwrap())));
        }
    }
    let tool = fixtures.iter().find(|f| f.name == "tool_call").unwrap();
    let body = tool.json.clone().unwrap();
    rejects(
        validate_anthropic_response(&with(body.clone(), "/stop_reason", json!("end_turn"))),
        "tool_use block is pending",
    );
    rejects(
        validate_anthropic_response(&with(body.clone(), "/content/0/input", json!("{}"))),
        "input must be an object",
    );
    rejects(
        validate_anthropic_response(&with(body.clone(), "/stop_sequence", json!("x"))),
        "only reported with",
    );
    rejects(
        validate_anthropic_response(&with(body.clone(), "/id", json!("chatcmpl-1"))),
        "msg_",
    );
    rejects(
        validate_anthropic_response(&with(body, "/stop_reason", json!("stop"))),
        "invalid value",
    );

    let events = parse_sse(tool.sse.unwrap());
    rejects(
        validate_anthropic_stream(&events[..events.len() - 1]),
        "message_stop",
    );
    rejects(
        validate_anthropic_stream(&events[1..]),
        "before message_start",
    );
    // The block is never closed.
    let mut open = events.clone();
    open.remove(5);
    rejects(validate_anthropic_stream(&open), "is open");
    // The input JSON is cut short.
    let mut cut = events.clone();
    cut.remove(4);
    rejects(
        validate_anthropic_stream(&cut),
        "not a complete JSON object",
    );
    let mut done = events;
    done.push(switchyard_core::SseEvent::data("[DONE]"));
    rejects(validate_anthropic_stream(&done), "after the end");
}

// ---------------------------------------------------------------------------
// Gemini
// ---------------------------------------------------------------------------

fn gemini_tool_loop() -> Value {
    json!({
        "systemInstruction": {"parts": [{"text": "You are a weather bot."}]},
        "contents": [
            {"role": "user", "parts": [{"text": "Weather in Paris and Tokyo?"}]},
            {"role": "model", "parts": [
                {"functionCall": {"name": "get_weather", "args": {"location": "Paris"}},
                 "thoughtSignature": "skip_thought_signature_validator"},
                {"functionCall": {"name": "get_weather", "args": {"location": "Tokyo"}}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "get_weather", "response": {"result": "cloudy"}}},
                {"functionResponse": {"name": "get_weather", "response": {"result": "sunny"}}},
                {"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgo="}}
            ]}
        ],
        "tools": [{"functionDeclarations": [{
            "name": "get_weather",
            "parametersJsonSchema": {"type": "object", "properties": {"location": {"type": "string"}}, "required": ["location"]}
        }]}],
        "toolConfig": {"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["get_weather"]}},
        "generationConfig": {"maxOutputTokens": 100, "thinkingConfig": {"thinkingBudget": 1024, "includeThoughts": true}}
    })
}

#[test]
fn gemini_request_known_good() {
    accepts(validate_gemini_request(&gemini_tool_loop()));
    accepts(validate_gemini_request(&json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {"responseMimeType": "application/json",
                             "responseJsonSchema": {"type": "object"},
                             "thinkingConfig": {"thinkingLevel": "low"}}
    })));
}

#[test]
fn gemini_request_known_bad() {
    let good = gemini_tool_loop();
    rejects(
        validate_gemini_request(&with(good.clone(), "/model", json!("gemini-2.5-pro"))),
        "unknown field `model`",
    );
    rejects(
        validate_gemini_request(&with(good.clone(), "/stream", json!(true))),
        "unknown field `stream`",
    );
    rejects(
        validate_gemini_request(&with(
            good.clone(),
            "/contents/1/parts/0/functionCall/name",
            json!("9lives"),
        )),
        "invalid function name",
    );
    rejects(
        validate_gemini_request(&with(
            good.clone(),
            "/contents/2/parts/0/functionResponse/response",
            json!("cloudy"),
        )),
        "response must be an object",
    );
    rejects(
        validate_gemini_request(&without(
            good.clone(),
            "/contents/1/parts/0/thoughtSignature",
        )),
        "carries no thoughtSignature",
    );
    rejects(
        validate_gemini_request(&with(
            good.clone(),
            "/contents/1/parts/0/thoughtSignature",
            json!("c3kxLmEuRXFRQkNr"),
        )),
        "gateway-wrapped",
    );
    rejects(
        validate_gemini_request(&with(
            good.clone(),
            "/contents/2/parts/0/thoughtSignature",
            json!("CiQB"),
        )),
        "must not carry a thought signature",
    );
    rejects(
        validate_gemini_request(&without(good.clone(), "/contents/2/parts/1")),
        "2 function call(s) but 1 response(s)",
    );
    rejects(
        validate_gemini_request(&with(
            good.clone(),
            "/contents/2/parts/1/functionResponse/name",
            json!("other"),
        )),
        "answered by a response named",
    );
    rejects(
        validate_gemini_request(&with(
            good.clone(),
            "/contents/2/parts/2",
            json!({"text": "and?"}),
        )),
        "text after a functionResponse",
    );
    rejects(
        validate_gemini_request(&with(good.clone(), "/contents/0/role", json!("model"))),
        "start with a user turn",
    );
    rejects(
        validate_gemini_request(&with(
            good.clone(),
            "/contents/3",
            json!({"role": "user", "parts": [{"text": "x"}]}),
        )),
        "two consecutive `user` turns",
    );
    rejects(
        validate_gemini_request(&with(
            good.clone(),
            "/tools/0/functionDeclarations/0/parametersJsonSchema",
            json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/x"}}, "$defs": {"x": {}}}),
        )),
        "unsupported schema keyword",
    );
    rejects(
        validate_gemini_request(&with(
            good.clone(),
            "/tools/0/functionDeclarations/0/parametersJsonSchema",
            json!({"type": "object", "properties": {"a": {"type": ["string", "null"]}}}),
        )),
        "single type name",
    );
    // A property *named* like a keyword is not a keyword.
    accepts(validate_gemini_request(&with(
        good.clone(),
        "/tools/0/functionDeclarations/0/parametersJsonSchema",
        json!({"type": "object", "properties": {"title": {"type": "string"}, "$ref": {"type": "string"}}}),
    )));
    rejects(
        validate_gemini_request(&with(
            good.clone(),
            "/generationConfig/thinkingConfig/thinkingLevel",
            json!("high"),
        )),
        "both set",
    );
    rejects(
        validate_gemini_request(&with(
            good.clone(),
            "/toolConfig/functionCallingConfig/allowedFunctionNames",
            json!(["other"]),
        )),
        "not declared",
    );
    rejects(
        validate_gemini_request(&with(
            good,
            "/contents/2/parts/2",
            json!({"inline_data": {"mime_type": "image/png", "data": "iVBORw0KGgo="}}),
        )),
        "unknown part field",
    );
}

#[test]
fn gemini_response_and_stream_known_good_and_bad() {
    let fixtures = scenarios::responses(Protocol::Gemini);
    for fixture in &fixtures {
        if let Some(body) = &fixture.json {
            accepts(validate_gemini_response(body));
        }
        if fixture.end != scenarios::StreamEnd::Truncated {
            accepts(validate_gemini_stream(&parse_sse(fixture.sse.unwrap())));
        }
    }
    let usage = fixtures.iter().find(|f| f.name == "text_usage").unwrap();
    let body = usage.json.clone().unwrap();
    rejects(
        validate_gemini_response(&with(
            body.clone(),
            "/usageMetadata/totalTokenCount",
            json!(107),
        )),
        "totalTokenCount",
    );
    rejects(
        validate_gemini_response(&with(
            body.clone(),
            "/usageMetadata/cachedContentTokenCount",
            json!(101),
        )),
        "exceeds promptTokenCount",
    );
    rejects(
        validate_gemini_response(&with(
            body.clone(),
            "/candidates/0/finishReason",
            json!("stop"),
        )),
        "invalid finishReason",
    );
    rejects(
        validate_gemini_response(&with(
            body.clone(),
            "/candidates/0/content/parts/0/thoughtSignature",
            json!("sy1.a.EqQB"),
        )),
        "not base64",
    );
    rejects(
        validate_gemini_response(&without(body, "/responseId")),
        "responseId",
    );

    let text = fixtures.iter().find(|f| f.name == "text").unwrap();
    let events = parse_sse(text.sse.unwrap());
    rejects(
        validate_gemini_stream(&events[..events.len() - 1]),
        "no chunk carries a finishReason",
    );
    let mut done = events.clone();
    done.push(switchyard_core::SseEvent::data("[DONE]"));
    rejects(validate_gemini_stream(&done), "[DONE]");
    let mut reversed = events;
    reversed.reverse();
    rejects(validate_gemini_stream(&reversed), "not on the last chunk");
}

// ---------------------------------------------------------------------------
// The scenario library itself
// ---------------------------------------------------------------------------

/// Every canned upstream response must be valid for its own vendor, or the
/// matrix would be feeding the decoders something no upstream sends.
#[test]
fn canned_chat_responses_are_valid() {
    for fixture in scenarios::responses(Protocol::OpenaiChat) {
        if let Some(body) = &fixture.json
            && let Err(violations) = validate_chat_response(body)
        {
            panic!("{}: {violations:#?}", fixture.name);
        }
    }
}

/// Request scenarios that are valid for their own vendor as written must
/// pass that vendor's validator untouched. (Scenarios that deliberately send
/// what the vendor refuses — empty contents, odd tool names, same-role runs —
/// are the ones the gateway has to repair.)
#[test]
fn native_request_scenarios_are_valid_for_their_own_vendor() {
    let repaired = [
        "empty_contents",
        "odd_tool_names",
        "long_conversation",
        "mid_system",
        "tiny_limit",
    ];
    for protocol in Protocol::ALL {
        // Gemini bodies use SDK spellings (snake_case, upper-case schema
        // types, missing roles) the strict validator does not model.
        if protocol == Protocol::Gemini {
            continue;
        }
        for scenario in scenarios::requests(protocol) {
            if repaired.contains(&scenario.name) {
                continue;
            }
            // Sampling scenarios deliberately carry values another vendor
            // must clamp; Anthropic's own pair of temperature + top_p is one.
            if protocol == Protocol::Anthropic && scenario.name == "sampling" {
                continue;
            }
            let report = support::harness::validate_request(protocol, &scenario.body);
            if let Err(violations) = report {
                panic!("{protocol} scenario `{}`: {violations:#?}", scenario.name);
            }
        }
    }
}
