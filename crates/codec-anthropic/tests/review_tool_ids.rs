//! Regression tests for review finding ANTH-4, written by the reviewer
//! against the defective code. The text below describes the behaviour
//! before the fix and is kept as the rationale for the tests.
//!
//! Review evidence: a call id reused by two different tool calls is written
//! twice.
//!
//! The Messages API requires `tool_use` ids to be unique across the request
//! (400 "`tool_use` ids must be unique"). Ids are only unique per vendor
//! convention: Moonshot/Kimi issue `functions.<name>:<index>` (identical in
//! every turn that calls the same function first), several local servers and
//! client frameworks count `call_0`, `call_1`, … from zero in every response.
//! `ToolIdMap` keeps *different* raw ids apart after sanitising, but maps the
//! same raw id to the same wire id every time, so such a history is emitted
//! with duplicate ids. The track asks for ids to be sanitised
//! "deterministically and consistently between tool_use and tool_result";
//! consistency per call (the n-th call with an id pairs with the n-th result
//! with that id) keeps both properties.

mod common;

use common::encode_request;
use serde_json::Value;
use std::collections::HashSet;
use switchyard_core::Protocol;
use switchyard_core::ir::{FunctionTool, Message, Part, Request, Role, Tool};

fn function_tool(name: &str) -> Tool {
    Tool::Function(FunctionTool {
        name: name.to_string(),
        description: None,
        parameters: Value::Null,
        strict: None,
        cache_control: None,
    })
}

fn blocks_of<'a>(message: &'a Value, kind: &str, id_key: &str) -> Vec<&'a str> {
    message["content"]
        .as_array()
        .expect("content")
        .iter()
        .filter(|block| block["type"] == kind)
        .filter_map(|block| block[id_key].as_str())
        .collect()
}

#[test]
fn review_reused_call_ids_do_not_become_duplicate_tool_use_ids() {
    let mut request = Request::new("claude-sonnet-4-5", Protocol::OpenaiChat);
    request.tools = vec![function_tool("get_weather")];
    request.messages = vec![
        Message::user_text("Weather in Paris?"),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call(
                "functions.get_weather:0",
                "get_weather",
                "{\"city\":\"Paris\"}",
            )],
        ),
        Message::new(
            Role::User,
            vec![Part::tool_result_text("functions.get_weather:0", "sunny")],
        ),
        Message::assistant_text("Sunny."),
        Message::user_text("And in Rome?"),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call(
                "functions.get_weather:0",
                "get_weather",
                "{\"city\":\"Rome\"}",
            )],
        ),
        Message::new(
            Role::User,
            vec![Part::tool_result_text("functions.get_weather:0", "rainy")],
        ),
    ];
    let body = encode_request(&request);
    let messages = body["messages"].as_array().expect("messages");

    let mut seen = HashSet::new();
    for message in messages {
        for id in blocks_of(message, "tool_use", "id") {
            assert!(
                seen.insert(id.to_string()),
                "`tool_use` id {id:?} appears twice; the API answers 400 \"`tool_use` ids must \
                 be unique\".\n{body:#}"
            );
        }
    }

    // Each result still answers the call of the turn right before it, with
    // its own content.
    for (at, message) in messages.iter().enumerate() {
        let results = blocks_of(message, "tool_result", "tool_use_id");
        if results.is_empty() {
            continue;
        }
        let calls = blocks_of(&messages[at - 1], "tool_use", "id");
        assert_eq!(results, calls, "results must pair with the preceding calls");
    }
    assert_eq!(messages[2]["content"][0]["content"], "sunny");
    assert_eq!(messages[6]["content"][0]["content"], "rainy");
}
