//! Regression tests for review finding ANTH-3, written by the reviewer
//! against the defective code. The text below describes the behaviour
//! before the fix and is kept as the rationale for the tests.
//!
//! Review evidence: manual (`enabled`) thinking is requested for a tool loop
//! whose last assistant turn has no thinking block.
//!
//! Vendor rule (notes 15 section 5.6, "Manual-mode extras"): "With `enabled`,
//! `tool_choice` must be `auto` or `none`, and the final assistant turn must
//! begin with a thinking block; adaptive mode drops that rule." The API
//! answers a violation with 400 ("Expected `thinking` or `redacted_thinking`,
//! but found `tool_use`. When `thinking` is enabled, a final `assistant`
//! message must start with a thinking block …").
//!
//! `encode_request` handles the first half of that sentence (forced tool
//! choice removes thinking) but not the second. The situation is the normal
//! shape of a translated tool loop whenever the signed thinking block is not
//! available: Chat Completions has no signature slot, and reasoning issued by
//! another vendor is (correctly) dropped by this encoder — which is exactly
//! what turns a valid history into an invalid one. DESIGN.md section 3:
//! `encode_request` "must keep the body valid".

mod common;

use common::encode_request_with;
use serde_json::Value;
use switchyard_core::ir::{FunctionTool, Message, Part, Reasoning, Request, Role, Signature, Tool};
use switchyard_core::reasoning::{Depth, ModelThinking, ReasoningConfig, ThinkingSupport};
use switchyard_core::{Protocol, UpstreamCtx};

fn function_tool(name: &str) -> Tool {
    Tool::Function(FunctionTool {
        name: name.to_string(),
        description: None,
        parameters: Value::Null,
        strict: None,
        cache_control: None,
    })
}

/// Claude Sonnet 4.5 as the catalog describes it: budgets only.
fn manual_only() -> ThinkingSupport {
    ThinkingSupport {
        min: 1024,
        max: 128_000,
        zero_allowed: true,
        dynamic_allowed: false,
        levels: Vec::new(),
    }
}

fn first_block_type(message: &Value) -> &str {
    message["content"][0]["type"].as_str().unwrap_or("")
}

fn assert_valid_for_manual_thinking(body: &Value) {
    let manual = body["thinking"]["type"] == "enabled";
    let messages = body["messages"].as_array().expect("messages");
    let last_assistant = messages
        .iter()
        .rev()
        .find(|message| message["role"] == "assistant")
        .expect("an assistant turn");
    let starts_with_thinking = matches!(
        first_block_type(last_assistant),
        "thinking" | "redacted_thinking"
    );
    assert!(
        !manual || starts_with_thinking,
        "thinking.type is \"enabled\" but the final assistant turn of the tool loop starts with \
         `{}`: the API rejects this body with 400.\n{body:#}",
        first_block_type(last_assistant)
    );
}

#[test]
fn review_enabled_thinking_is_not_sent_when_the_tool_turn_has_no_thinking_block() {
    // Chat Completions client, second request of a tool loop.
    let caps = manual_only();
    let ctx = UpstreamCtx {
        thinking: ModelThinking::Supported(&caps),
        max_output_tokens: Some(64_000),
        ..UpstreamCtx::default()
    };
    let mut request = Request::new("claude-sonnet-4-5", Protocol::OpenaiChat);
    request.tools = vec![function_tool("lookup")];
    request.reasoning = Some(ReasoningConfig::with_depth(Depth::Budget(8192)));
    request.messages = vec![
        Message::user_text("What is the answer?"),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call("call_1", "lookup", "{}")],
        ),
        Message::new(Role::User, vec![Part::tool_result_text("call_1", "42")]),
    ];
    assert_valid_for_manual_thinking(&encode_request_with(&request, &ctx));
}

#[test]
fn review_dropping_foreign_reasoning_must_not_leave_enabled_thinking_behind() {
    // Messages client whose previous attempt was served by Gemini: the
    // thinking block carries a Gemini signature, the encoder drops it, and
    // the turn now starts with tool_use.
    let caps = manual_only();
    let ctx = UpstreamCtx {
        thinking: ModelThinking::Supported(&caps),
        max_output_tokens: Some(64_000),
        ..UpstreamCtx::default()
    };
    let mut request = Request::new("claude-sonnet-4-5", Protocol::Anthropic);
    request.tools = vec![function_tool("lookup")];
    request.max_output_tokens = Some(16_000);
    request.reasoning = Some(ReasoningConfig::with_depth(Depth::Budget(8192)));
    request.messages = vec![
        Message::user_text("What is the answer?"),
        Message::new(
            Role::Assistant,
            vec![
                Part::Reasoning(Reasoning {
                    id: None,
                    text: "I should look it up.".into(),
                    signature: Some(Signature::new(Protocol::Gemini, "CpsBAVSoXO4")),
                    redacted: false,
                }),
                Part::tool_call("toolu_1", "lookup", "{}"),
            ],
        ),
        Message::new(Role::User, vec![Part::tool_result_text("toolu_1", "42")]),
    ];
    assert_valid_for_manual_thinking(&encode_request_with(&request, &ctx));
}

#[test]
fn review_enabled_thinking_stays_when_the_tool_turn_starts_with_signed_thinking() {
    // Guard for the fix: a replayable block keeps thinking on.
    let caps = manual_only();
    let ctx = UpstreamCtx {
        thinking: ModelThinking::Supported(&caps),
        max_output_tokens: Some(64_000),
        ..UpstreamCtx::default()
    };
    let mut request = Request::new("claude-sonnet-4-5", Protocol::Anthropic);
    request.tools = vec![function_tool("lookup")];
    request.max_output_tokens = Some(16_000);
    request.reasoning = Some(ReasoningConfig::with_depth(Depth::Budget(8192)));
    request.messages = vec![
        Message::user_text("What is the answer?"),
        Message::new(
            Role::Assistant,
            vec![
                Part::Reasoning(Reasoning {
                    id: None,
                    text: "I should look it up.".into(),
                    signature: Some(Signature::new(Protocol::Anthropic, "EqQBCkYIBBgCKkD")),
                    redacted: false,
                }),
                Part::tool_call("toolu_1", "lookup", "{}"),
            ],
        ),
        Message::new(Role::User, vec![Part::tool_result_text("toolu_1", "42")]),
    ];
    let body = encode_request_with(&request, &ctx);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_valid_for_manual_thinking(&body);
}
