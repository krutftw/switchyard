//! Regression tests for review finding ANTH-1, written by the reviewer
//! against the defective code. The text below describes the behaviour
//! before the fix and is kept as the rationale for the tests.
//!
//! Review evidence: whitespace-only text blocks reach the upstream.
//!
//! The Messages API validates every text block and answers 400
//! `invalid_request_error` ("text content blocks must contain non-whitespace
//! text") when a block's text is empty *or consists only of whitespace*.
//! `encode_request` filters empty text but forwards whitespace-only text.
//!
//! Such parts are ordinary in histories that come from other protocols: Chat
//! Completions assistants routinely carry `content: "\n\n"` (or `" "`) next
//! to `tool_calls`, and Gemini emits whitespace-only text parts around
//! function calls. Because a 400 is a request fault there is no failover, and
//! because the block stays in the history every later turn fails as well.

mod common;

use common::encode_request;
use serde_json::Value;
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

/// Every `text` block found in `messages[*].content`.
fn message_text_blocks(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .filter(|block| block["type"] == "text")
        .map(|block| block["text"].as_str().unwrap_or("").to_string())
        .collect()
}

#[test]
fn review_whitespace_only_text_next_to_a_tool_call_is_not_sent() {
    // What a Chat Completions client replays after a tool call:
    //   {"role":"assistant","content":"\n\n","tool_calls":[…]}
    //   {"role":"tool","tool_call_id":"call_1","content":"42"}
    let mut request = Request::new("claude-sonnet-4-5", Protocol::OpenaiChat);
    request.tools = vec![function_tool("lookup")];
    request.messages = vec![
        Message::user_text("What is the answer?"),
        Message::new(
            Role::Assistant,
            vec![
                Part::text("\n\n"),
                Part::tool_call("call_1", "lookup", "{}"),
            ],
        ),
        Message::new(
            Role::User,
            vec![Part::tool_result_text("call_1", "42"), Part::text("  ")],
        ),
    ];
    let body = encode_request(&request);
    let texts = message_text_blocks(&body);
    let blank: Vec<&String> = texts.iter().filter(|t| t.trim().is_empty()).collect();
    assert!(
        blank.is_empty(),
        "whitespace-only text blocks are rejected by the API (400 \"text content blocks must \
         contain non-whitespace text\"), found {blank:?} in {body:#}"
    );
    // The tool exchange itself must survive.
    assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
}

#[test]
fn review_whitespace_only_system_block_is_not_sent() {
    let mut request = Request::new("claude-sonnet-4-5", Protocol::OpenaiChat);
    request.system = vec![Part::text("You are terse."), Part::text(" \n\t")];
    request.messages = vec![Message::user_text("hi")];
    let body = encode_request(&request);
    let system = body["system"].as_array().expect("system blocks");
    let blank: Vec<&Value> = system
        .iter()
        .filter(|block| block["text"].as_str().unwrap_or("").trim().is_empty())
        .collect();
    assert!(
        blank.is_empty(),
        "whitespace-only system text blocks are rejected by the API, found {blank:?}"
    );
    assert_eq!(system.len(), 1);
}
