//! Stream decoder: vendor `chat.completion.chunk` transcripts into canonical
//! events. Every sequence is checked against the stream contract by
//! `common::decode_stream`.

mod common;

use common::{accumulate, decode_stream, sse};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_chat::ChatCodec;
use switchyard_core::codec::Codec;
use switchyard_core::error::ErrorKind;
use switchyard_core::ir::{
    Citation, FinishReason, MediaPart, Part, Reasoning, RefusalPart, Response, Signature, ToolCall,
    ToolCallKind,
};
use switchyard_core::stream::{BlockStart, StreamEvent};
use switchyard_core::{Protocol, Usage};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// One `data:` line holding a chunk with a single choice.
fn chunk(delta: Value, finish: Value) -> String {
    let chunk = json!({
        "id": "chatcmpl-T1",
        "object": "chat.completion.chunk",
        "created": 1741570002,
        "model": "gpt-4o",
        "choices": [{"index": 0, "delta": delta, "logprobs": null, "finish_reason": finish}]
    });
    format!("data: {chunk}\n\n")
}

fn delta(delta: Value) -> String {
    chunk(delta, Value::Null)
}

fn finish(reason: &str) -> String {
    chunk(json!({}), json!(reason))
}

const DONE: &str = "data: [DONE]\n\n";

fn tool_delta(index: i64, id: Option<&str>, name: Option<&str>, arguments: &str) -> String {
    let mut call = json!({"index": index, "function": {"arguments": arguments}});
    if let Some(id) = id {
        call["id"] = json!(id);
        call["type"] = json!("function");
    }
    if let Some(name) = name {
        call["function"]["name"] = json!(name);
    }
    delta(json!({"tool_calls": [call]}))
}

fn start() -> StreamEvent {
    StreamEvent::Start {
        id: "chatcmpl-T1".into(),
        model: "gpt-4o".into(),
        created: 1741570002,
    }
}

fn block_start(index: u32, block: BlockStart) -> StreamEvent {
    StreamEvent::BlockStart { index, block }
}

fn tool_start(index: u32, id: &str, name: &str) -> StreamEvent {
    block_start(
        index,
        BlockStart::ToolCall {
            id: id.into(),
            name: name.into(),
            kind: ToolCallKind::Function,
            signature: None,
        },
    )
}

fn reasoning_start(index: u32) -> StreamEvent {
    block_start(
        index,
        BlockStart::Reasoning {
            id: None,
            redacted: false,
        },
    )
}

fn text(index: u32, text: &str) -> StreamEvent {
    StreamEvent::TextDelta {
        index,
        text: text.into(),
    }
}

fn thought(index: u32, text: &str) -> StreamEvent {
    StreamEvent::ReasoningDelta {
        index,
        text: text.into(),
    }
}

fn args(index: u32, fragment: &str) -> StreamEvent {
    StreamEvent::ToolArgsDelta {
        index,
        fragment: fragment.into(),
    }
}

fn stop(index: u32) -> StreamEvent {
    StreamEvent::BlockStop { index }
}

fn finished(reason: FinishReason) -> StreamEvent {
    StreamEvent::Finish {
        reason,
        stop_sequence: None,
    }
}

fn expected_response(parts: Vec<Part>, finish: FinishReason, usage: Usage) -> Response {
    Response {
        id: "chatcmpl-T1".into(),
        model: "gpt-4o".into(),
        created: 1741570002,
        parts,
        finish,
        stop_sequence: None,
        usage,
        service_tier: None,
    }
}

// ---------------------------------------------------------------------------
// Vendor transcripts
// ---------------------------------------------------------------------------

/// OpenAI, `stream_options.include_usage: true`.
const OPENAI_TEXT: &str = r#"data: {"id":"chatcmpl-B9MHDbslfkBeAs8l4bebGdFOJ6PeG","object":"chat.completion.chunk","created":1741570283,"model":"gpt-4o-2024-08-06","service_tier":"default","system_fingerprint":"fp_fc9f1d7035","choices":[{"index":0,"delta":{"role":"assistant","content":"","refusal":null},"logprobs":null,"finish_reason":null}],"usage":null}

data: {"id":"chatcmpl-B9MHDbslfkBeAs8l4bebGdFOJ6PeG","object":"chat.completion.chunk","created":1741570283,"model":"gpt-4o-2024-08-06","service_tier":"default","system_fingerprint":"fp_fc9f1d7035","choices":[{"index":0,"delta":{"content":"Hello"},"logprobs":null,"finish_reason":null}],"usage":null}

data: {"id":"chatcmpl-B9MHDbslfkBeAs8l4bebGdFOJ6PeG","object":"chat.completion.chunk","created":1741570283,"model":"gpt-4o-2024-08-06","service_tier":"default","system_fingerprint":"fp_fc9f1d7035","choices":[{"index":0,"delta":{"content":" world"},"logprobs":null,"finish_reason":null}],"usage":null}

data: {"id":"chatcmpl-B9MHDbslfkBeAs8l4bebGdFOJ6PeG","object":"chat.completion.chunk","created":1741570283,"model":"gpt-4o-2024-08-06","service_tier":"default","system_fingerprint":"fp_fc9f1d7035","choices":[{"index":0,"delta":{},"logprobs":null,"finish_reason":"stop"}],"usage":null}

data: {"id":"chatcmpl-B9MHDbslfkBeAs8l4bebGdFOJ6PeG","object":"chat.completion.chunk","created":1741570283,"model":"gpt-4o-2024-08-06","service_tier":"default","system_fingerprint":"fp_fc9f1d7035","choices":[],"usage":{"prompt_tokens":9,"completion_tokens":2,"total_tokens":11,"prompt_tokens_details":{"cached_tokens":0,"audio_tokens":0},"completion_tokens_details":{"reasoning_tokens":0,"audio_tokens":0,"accepted_prediction_tokens":0,"rejected_prediction_tokens":0}}}

data: [DONE]

"#;

#[test]
fn openai_text_only_with_usage_chunk() {
    let events = decode_stream(OPENAI_TEXT);
    let usage = Usage {
        input_tokens: 9,
        output_tokens: 2,
        ..Usage::default()
    };
    assert_eq!(
        events,
        vec![
            StreamEvent::Start {
                id: "chatcmpl-B9MHDbslfkBeAs8l4bebGdFOJ6PeG".into(),
                model: "gpt-4o-2024-08-06".into(),
                created: 1741570283,
            },
            block_start(0, BlockStart::Text),
            text(0, "Hello"),
            text(0, " world"),
            stop(0),
            // The usage-only chunk arrives after the finish reason and is
            // still reported before `Finish`.
            StreamEvent::Usage(usage),
            finished(FinishReason::Stop),
        ]
    );
    let response = accumulate(&events);
    assert_eq!(response.text(), "Hello world");
    assert_eq!(response.usage, usage);
}

/// DeepSeek: `reasoning_content` before `content`, usage on the finish chunk.
const DEEPSEEK_REASONING: &str = r#"data: {"id":"7f1c4b7e-2a55-4f2b-9d5e-0c1d2e3f4a5b","object":"chat.completion.chunk","created":1741570300,"model":"deepseek-reasoner","system_fingerprint":"fp_5417b77867_prod","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":""},"logprobs":null,"finish_reason":null}]}

data: {"id":"7f1c4b7e-2a55-4f2b-9d5e-0c1d2e3f4a5b","object":"chat.completion.chunk","created":1741570300,"model":"deepseek-reasoner","system_fingerprint":"fp_5417b77867_prod","choices":[{"index":0,"delta":{"content":null,"reasoning_content":"Simple"},"logprobs":null,"finish_reason":null}]}

data: {"id":"7f1c4b7e-2a55-4f2b-9d5e-0c1d2e3f4a5b","object":"chat.completion.chunk","created":1741570300,"model":"deepseek-reasoner","system_fingerprint":"fp_5417b77867_prod","choices":[{"index":0,"delta":{"content":null,"reasoning_content":" addition."},"logprobs":null,"finish_reason":null}]}

data: {"id":"7f1c4b7e-2a55-4f2b-9d5e-0c1d2e3f4a5b","object":"chat.completion.chunk","created":1741570300,"model":"deepseek-reasoner","system_fingerprint":"fp_5417b77867_prod","choices":[{"index":0,"delta":{"content":"2 + 2","reasoning_content":null},"logprobs":null,"finish_reason":null}]}

data: {"id":"7f1c4b7e-2a55-4f2b-9d5e-0c1d2e3f4a5b","object":"chat.completion.chunk","created":1741570300,"model":"deepseek-reasoner","system_fingerprint":"fp_5417b77867_prod","choices":[{"index":0,"delta":{"content":" = 4","reasoning_content":null},"logprobs":null,"finish_reason":null}]}

data: {"id":"7f1c4b7e-2a55-4f2b-9d5e-0c1d2e3f4a5b","object":"chat.completion.chunk","created":1741570300,"model":"deepseek-reasoner","system_fingerprint":"fp_5417b77867_prod","choices":[{"index":0,"delta":{"content":"","reasoning_content":null},"logprobs":null,"finish_reason":"stop"}],"usage":{"prompt_tokens":112,"completion_tokens":40,"total_tokens":152,"prompt_tokens_details":{"cached_tokens":64},"completion_tokens_details":{"reasoning_tokens":30},"prompt_cache_hit_tokens":64,"prompt_cache_miss_tokens":48}}

data: [DONE]

"#;

#[test]
fn deepseek_reasoning_then_text() {
    let events = decode_stream(DEEPSEEK_REASONING);
    let usage = Usage {
        input_tokens: 48,
        cache_read_tokens: 64,
        cache_write_tokens: 0,
        output_tokens: 40,
        reasoning_tokens: 30,
    };
    assert_eq!(
        events,
        vec![
            StreamEvent::Start {
                id: "7f1c4b7e-2a55-4f2b-9d5e-0c1d2e3f4a5b".into(),
                model: "deepseek-reasoner".into(),
                created: 1741570300,
            },
            reasoning_start(0),
            thought(0, "Simple"),
            thought(0, " addition."),
            stop(0),
            block_start(1, BlockStart::Text),
            text(1, "2 + 2"),
            text(1, " = 4"),
            stop(1),
            StreamEvent::Usage(usage),
            finished(FinishReason::Stop),
        ]
    );
    let response = accumulate(&events);
    assert_eq!(
        response.parts,
        vec![Part::reasoning("Simple addition."), Part::text("2 + 2 = 4")]
    );
    assert_eq!(response.usage, usage);
}

/// OpenAI: text, then two parallel tool calls whose arguments are fragmented.
const OPENAI_TOOLS: &str = r#"data: {"id":"chatcmpl-T1","object":"chat.completion.chunk","created":1741570002,"model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":"","refusal":null},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-T1","object":"chat.completion.chunk","created":1741570002,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"I'll check both."},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-T1","object":"chat.completion.chunk","created":1741570002,"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":""}}]},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-T1","object":"chat.completion.chunk","created":1741570002,"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"ci"}}]},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-T1","object":"chat.completion.chunk","created":1741570002,"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ty\":\"Paris\"}"}}]},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-T1","object":"chat.completion.chunk","created":1741570002,"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_def","type":"function","function":{"name":"get_weather","arguments":""}}]},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-T1","object":"chat.completion.chunk","created":1741570002,"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"city\":"}}]},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-T1","object":"chat.completion.chunk","created":1741570002,"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"\"Rome\"}"}}]},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-T1","object":"chat.completion.chunk","created":1741570002,"model":"gpt-4o","choices":[{"index":0,"delta":{},"logprobs":null,"finish_reason":"tool_calls"}]}

data: [DONE]

"#;

#[test]
fn openai_text_and_parallel_tool_calls_stream_incrementally() {
    let events = decode_stream(OPENAI_TOOLS);
    assert_eq!(
        events,
        vec![
            start(),
            block_start(0, BlockStart::Text),
            text(0, "I'll check both."),
            stop(0),
            tool_start(1, "call_abc", "get_weather"),
            args(1, "{\"ci"),
            args(1, "ty\":\"Paris\"}"),
            // The first call's arguments are complete JSON, so the second
            // call can be announced and streamed live as well.
            stop(1),
            tool_start(2, "call_def", "get_weather"),
            args(2, "{\"city\":"),
            args(2, "\"Rome\"}"),
            stop(2),
            finished(FinishReason::ToolCalls),
        ]
    );
    assert_eq!(
        accumulate(&events),
        expected_response(
            vec![
                Part::text("I'll check both."),
                Part::tool_call("call_abc", "get_weather", "{\"city\":\"Paris\"}"),
                Part::tool_call("call_def", "get_weather", "{\"city\":\"Rome\"}"),
            ],
            FinishReason::ToolCalls,
            Usage::default(),
        )
    );
}

/// OpenRouter relaying Claude: `reasoning` plus `reasoning_details`, the
/// signature in a detail of its own.
const OPENROUTER_SIGNED_REASONING: &str = r#": OPENROUTER PROCESSING

data: {"id":"gen-1741570400-abc","provider":"Anthropic","model":"anthropic/claude-sonnet-4.5","object":"chat.completion.chunk","created":1741570400,"choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning":"Let me","reasoning_details":[{"type":"reasoning.text","text":"Let me","format":"anthropic-claude-v1","index":0}]},"finish_reason":null,"native_finish_reason":null,"logprobs":null}]}

data: {"id":"gen-1741570400-abc","provider":"Anthropic","model":"anthropic/claude-sonnet-4.5","object":"chat.completion.chunk","created":1741570400,"choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning":" think.","reasoning_details":[{"type":"reasoning.text","text":" think.","format":"anthropic-claude-v1","index":0}]},"finish_reason":null,"native_finish_reason":null,"logprobs":null}]}

data: {"id":"gen-1741570400-abc","provider":"Anthropic","model":"anthropic/claude-sonnet-4.5","object":"chat.completion.chunk","created":1741570400,"choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning_details":[{"type":"reasoning.text","signature":"ErUBCkYIBxgCIkD","format":"anthropic-claude-v1","index":0}]},"finish_reason":null,"native_finish_reason":null,"logprobs":null}]}

data: {"id":"gen-1741570400-abc","provider":"Anthropic","model":"anthropic/claude-sonnet-4.5","object":"chat.completion.chunk","created":1741570400,"choices":[{"index":0,"delta":{"role":"assistant","content":"Answer."},"finish_reason":null,"native_finish_reason":null,"logprobs":null}]}

data: {"id":"gen-1741570400-abc","provider":"Anthropic","model":"anthropic/claude-sonnet-4.5","object":"chat.completion.chunk","created":1741570400,"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":"stop","native_finish_reason":"end_turn","logprobs":null}]}

data: {"id":"gen-1741570400-abc","provider":"Anthropic","model":"anthropic/claude-sonnet-4.5","object":"chat.completion.chunk","created":1741570400,"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null,"native_finish_reason":null,"logprobs":null}],"usage":{"prompt_tokens":20,"completion_tokens":35,"total_tokens":55,"completion_tokens_details":{"reasoning_tokens":12}}}

data: [DONE]

"#;

#[test]
fn openrouter_reasoning_details_with_signature() {
    let events = decode_stream(OPENROUTER_SIGNED_REASONING);
    let signature = Signature::new(Protocol::OpenaiChat, "ErUBCkYIBxgCIkD");
    let usage = Usage {
        input_tokens: 20,
        output_tokens: 35,
        reasoning_tokens: 12,
        ..Usage::default()
    };
    assert_eq!(
        events,
        vec![
            StreamEvent::Start {
                id: "gen-1741570400-abc".into(),
                model: "anthropic/claude-sonnet-4.5".into(),
                created: 1741570400,
            },
            reasoning_start(0),
            // The text is sent twice per chunk (`reasoning` and the detail);
            // it is decoded once.
            thought(0, "Let me"),
            thought(0, " think."),
            StreamEvent::ReasoningSignature {
                index: 0,
                signature: signature.clone()
            },
            stop(0),
            block_start(1, BlockStart::Text),
            text(1, "Answer."),
            stop(1),
            StreamEvent::Usage(usage),
            finished(FinishReason::Stop),
        ]
    );
    assert_eq!(
        accumulate(&events).parts,
        vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: "Let me think.".into(),
                signature: Some(signature),
                redacted: false,
            }),
            Part::text("Answer."),
        ]
    );
}

// ---------------------------------------------------------------------------
// Tool-call shapes
// ---------------------------------------------------------------------------

#[test]
fn interleaved_tool_call_fragments_are_buffered_into_sequential_blocks() {
    let transcript = [
        tool_delta(0, Some("call_a"), Some("alpha"), ""),
        tool_delta(1, Some("call_b"), Some("beta"), ""),
        tool_delta(0, None, None, "{\"x\":"),
        tool_delta(1, None, None, "{\"y\":"),
        tool_delta(0, None, None, "1}"),
        tool_delta(1, None, None, "2}"),
        finish("tool_calls"),
        DONE.to_string(),
    ]
    .concat();
    let events = decode_stream(&transcript);
    assert_eq!(
        events,
        vec![
            start(),
            tool_start(0, "call_a", "alpha"),
            args(0, "{\"x\":"),
            args(0, "1}"),
            stop(0),
            // Everything received for the second call while the first was
            // open is replayed in one fragment.
            tool_start(1, "call_b", "beta"),
            args(1, "{\"y\":2}"),
            stop(1),
            finished(FinishReason::ToolCalls),
        ]
    );
}

#[test]
fn interleaved_calls_that_never_complete_are_flushed_in_index_order() {
    let transcript = [
        tool_delta(1, Some("call_b"), Some("beta"), "{\"y\""),
        tool_delta(0, Some("call_a"), Some("alpha"), "{\"x\""),
        tool_delta(2, Some("call_c"), Some("gamma"), "{\"z\""),
        tool_delta(1, None, None, ":2}"),
        tool_delta(2, None, None, ":3}"),
        // The first call's arguments are cut short by the token limit.
        finish("length"),
        DONE.to_string(),
    ]
    .concat();
    let events = decode_stream(&transcript);
    let response = accumulate(&events);
    assert_eq!(
        response.parts,
        vec![
            // Announced first because it arrived first ...
            Part::tool_call("call_b", "beta", "{\"y\":2}"),
            // ... the rest waited and comes out in index order.
            Part::tool_call("call_a", "alpha", "{\"x\""),
            Part::tool_call("call_c", "gamma", "{\"z\":3}"),
        ]
    );
    assert_eq!(response.finish, FinishReason::Length);
}

#[test]
fn two_calls_in_one_chunk() {
    let transcript = [
        delta(json!({"role": "assistant", "content": null})),
        delta(json!({"tool_calls": [
            {"index": 0, "id": "call_a", "type": "function",
             "function": {"name": "alpha", "arguments": "{\"x\":1}"}},
            {"index": 1, "id": "call_b", "type": "function",
             "function": {"name": "beta", "arguments": "{\"y\":2}"}}
        ]})),
        finish("tool_calls"),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        decode_stream(&transcript),
        vec![
            start(),
            tool_start(0, "call_a", "alpha"),
            args(0, "{\"x\":1}"),
            stop(0),
            tool_start(1, "call_b", "beta"),
            args(1, "{\"y\":2}"),
            stop(1),
            finished(FinishReason::ToolCalls),
        ]
    );
}

#[test]
fn arguments_before_id_and_name_then_repeated_headers() {
    let transcript = [
        // Arguments arrive first, without id or name.
        delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"a\""}}]})),
        tool_delta(0, Some("call_late"), Some("lookup"), ":1"),
        // Some servers repeat id and name on every fragment.
        tool_delta(0, Some("call_late"), Some("lookup"), "}"),
        finish("tool_calls"),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        decode_stream(&transcript),
        vec![
            start(),
            tool_start(0, "call_late", "lookup"),
            args(0, "{\"a\":1"),
            args(0, "}"),
            stop(0),
            finished(FinishReason::ToolCalls),
        ]
    );
}

#[test]
fn id_arriving_after_the_name() {
    let transcript = [
        delta(
            json!({"tool_calls": [{"index": 0, "function": {"name": "lookup", "arguments": ""}}]}),
        ),
        delta(json!({"tool_calls": [{"index": 0, "id": "", "function": {"arguments": "{}"}}]})),
        delta(json!({"tool_calls": [{"index": 0, "id": "call_real"}]})),
        finish("tool_calls"),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        decode_stream(&transcript),
        vec![
            start(),
            tool_start(0, "call_real", "lookup"),
            args(0, "{}"),
            stop(0),
            finished(FinishReason::ToolCalls),
        ]
    );
}

#[test]
fn calls_without_any_id_get_one_minted() {
    let transcript = [
        delta(json!({"tool_calls": [
            {"index": 1, "function": {"name": "second", "arguments": "{}"}},
            {"index": 0, "function": {"name": "first", "arguments": "{}"}}
        ]})),
        finish("tool_calls"),
        DONE.to_string(),
    ]
    .concat();
    let response = accumulate(&decode_stream(&transcript));
    let calls: Vec<&ToolCall> = response.tool_calls().collect();
    assert_eq!(
        calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert!(
        calls
            .iter()
            .all(|c| c.id.starts_with("call_") && c.id.len() == 29)
    );
    assert_ne!(calls[0].id, calls[1].id);
}

#[test]
fn whole_calls_that_all_claim_index_zero_are_separate_calls() {
    // Ollama and a few other servers send complete calls, each with index 0.
    let transcript = [
        delta(
            json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
            "function": {"name": "alpha", "arguments": "{\"x\":1}"}}]}),
        ),
        delta(
            json!({"tool_calls": [{"index": 0, "id": "call_2", "type": "function",
            "function": {"name": "beta", "arguments": {"y": 2}}}]}),
        ),
        // ... and `stop` although the turn ends in tool calls.
        finish("stop"),
        DONE.to_string(),
    ]
    .concat();
    let response = accumulate(&decode_stream(&transcript));
    assert_eq!(
        response.parts,
        vec![
            Part::tool_call("call_1", "alpha", "{\"x\":1}"),
            // Arguments sent as an object are serialised.
            Part::tool_call("call_2", "beta", "{\"y\":2}"),
        ]
    );
    assert_eq!(response.finish, FinishReason::ToolCalls);
}

#[test]
fn calls_without_index_use_their_array_position() {
    let transcript = [
        delta(json!({"tool_calls": [
            {"id": "call_1", "function": {"name": "alpha", "arguments": "{}"}},
            {"id": "call_2", "function": {"name": "beta", "arguments": "{}"}}
        ]})),
        finish("tool_calls"),
    ]
    .concat();
    let response = accumulate(&decode_stream(&transcript));
    assert_eq!(
        response.parts,
        vec![
            Part::tool_call("call_1", "alpha", "{}"),
            Part::tool_call("call_2", "beta", "{}")
        ]
    );
}

#[test]
fn nameless_calls_are_dropped_and_do_not_fake_a_tool_turn() {
    let transcript = [
        delta(json!({"content": "hi"})),
        delta(
            json!({"tool_calls": [{"index": 0, "id": "call_x", "function": {"arguments": "{}"}}]}),
        ),
        finish("tool_calls"),
        DONE.to_string(),
    ]
    .concat();
    let response = accumulate(&decode_stream(&transcript));
    assert_eq!(response.parts, vec![Part::text("hi")]);
    assert_eq!(response.finish, FinishReason::Stop);
}

#[test]
fn empty_tool_call_deltas_do_not_open_anything() {
    let transcript = [
        delta(json!({"content": "a", "tool_calls": []})),
        delta(json!({"content": "b", "tool_calls": [{"index": 0, "function": {"arguments": ""}}]})),
        delta(json!({"content": "c", "tool_calls": null})),
        finish("stop"),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        decode_stream(&transcript),
        vec![
            start(),
            block_start(0, BlockStart::Text),
            text(0, "a"),
            text(0, "b"),
            text(0, "c"),
            stop(0),
            finished(FinishReason::Stop),
        ]
    );
}

#[test]
fn custom_tool_call_stream() {
    let transcript = [
        delta(
            json!({"tool_calls": [{"index": 0, "id": "call_c", "type": "custom",
            "custom": {"name": "run_sql", "input": ""}}]}),
        ),
        delta(json!({"tool_calls": [{"index": 0, "custom": {"input": "SELECT "}}]})),
        delta(json!({"tool_calls": [{"index": 0, "custom": {"input": "1"}}]})),
        finish("tool_calls"),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        accumulate(&decode_stream(&transcript)).parts,
        vec![Part::ToolCall(ToolCall {
            id: "call_c".into(),
            name: "run_sql".into(),
            arguments: "SELECT 1".into(),
            kind: ToolCallKind::Custom,
            signature: None,
            cache_control: None,
        })]
    );
}

#[test]
fn legacy_function_call_stream() {
    let transcript = [
        delta(json!({"role": "assistant", "content": null,
                     "function_call": {"name": "get_time", "arguments": ""}})),
        delta(json!({"function_call": {"arguments": "{\"tz\":"}})),
        delta(json!({"function_call": {"arguments": "\"UTC\"}"}})),
        finish("function_call"),
        DONE.to_string(),
    ]
    .concat();
    let response = accumulate(&decode_stream(&transcript));
    let call = response.tool_calls().next().expect("a call");
    assert_eq!(
        (call.name.as_str(), call.arguments.as_str()),
        ("get_time", "{\"tz\":\"UTC\"}")
    );
    assert!(call.id.starts_with("call_"));
    assert_eq!(response.finish, FinishReason::ToolCalls);
}

#[test]
fn tool_call_thought_signature_is_tagged_as_chat_origin() {
    // Google's OpenAI-compatible endpoint.
    let transcript = [
        delta(json!({"tool_calls": [{
            "index": 0, "id": "call_g", "type": "function",
            "function": {"name": "f", "arguments": "{}"},
            "extra_content": {"google": {"thought_signature": "CiQBsig"}}
        }]})),
        finish("tool_calls"),
        DONE.to_string(),
    ]
    .concat();
    let events = decode_stream(&transcript);
    assert_eq!(
        events[1],
        block_start(
            0,
            BlockStart::ToolCall {
                id: "call_g".into(),
                name: "f".into(),
                kind: ToolCallKind::Function,
                signature: Some(Signature::new(Protocol::OpenaiChat, "CiQBsig")),
            }
        )
    );
}

#[test]
fn text_after_a_finished_call_gets_its_own_block() {
    let transcript = [
        tool_delta(0, Some("call_a"), Some("alpha"), "{}"),
        delta(json!({"content": "Calling alpha."})),
        finish("tool_calls"),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        decode_stream(&transcript),
        vec![
            start(),
            tool_start(0, "call_a", "alpha"),
            args(0, "{}"),
            stop(0),
            block_start(1, BlockStart::Text),
            text(1, "Calling alpha."),
            stop(1),
            finished(FinishReason::ToolCalls),
        ]
    );
}

#[test]
fn text_and_reasoning_during_an_unfinished_call_are_buffered_behind_it() {
    let transcript = [
        tool_delta(0, Some("call_a"), Some("bash"), "{\"command\":"),
        delta(json!({"content": "Note A: "})),
        delta(json!({"content": "running check"})),
        delta(json!({"reasoning_content": "Thinking about safety"})),
        delta(json!({"content": "Note B"})),
        tool_delta(0, None, None, "\"pwd\"}"),
        finish("tool_calls"),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        decode_stream(&transcript),
        vec![
            start(),
            tool_start(0, "call_a", "bash"),
            args(0, "{\"command\":"),
            args(0, "\"pwd\"}"),
            stop(0),
            block_start(1, BlockStart::Text),
            text(1, "Note A: running check"),
            stop(1),
            reasoning_start(2),
            thought(2, "Thinking about safety"),
            stop(2),
            block_start(3, BlockStart::Text),
            text(3, "Note B"),
            stop(3),
            finished(FinishReason::ToolCalls),
        ]
    );
}

// ---------------------------------------------------------------------------
// Reasoning, refusal, citations, images
// ---------------------------------------------------------------------------

#[test]
fn reasoning_spellings_in_deltas() {
    let parts = |deltas: &[Value]| {
        let mut transcript: String = deltas.iter().cloned().map(delta).collect();
        transcript.push_str(&finish("stop"));
        transcript.push_str(DONE);
        accumulate(&decode_stream(&transcript)).parts
    };
    let expected = vec![Part::reasoning("think"), Part::text("4")];
    // OpenRouter / Ollama: `reasoning` string.
    assert_eq!(
        parts(&[
            json!({"reasoning": "thi"}),
            json!({"reasoning": "nk"}),
            json!({"content": "4"})
        ]),
        expected
    );
    // `reasoning_content` wins; an empty or null one falls back to `reasoning`.
    assert_eq!(
        parts(&[
            json!({"reasoning_content": "think", "reasoning": "ignored"}),
            json!({"reasoning_content": null, "content": "4"})
        ]),
        expected
    );
    assert_eq!(
        parts(&[
            json!({"reasoning_content": "", "reasoning": "think"}),
            json!({"content": "4"})
        ]),
        expected
    );
    // Details only.
    assert_eq!(
        parts(&[
            json!({"reasoning_details": [{"type": "reasoning.text", "text": "think", "index": 0}]}),
            json!({"content": "4"})
        ]),
        expected
    );
    // Mistral: typed content items.
    assert_eq!(
        parts(&[
            json!({"content": [{"type": "thinking", "thinking": [{"type": "text", "text": "think"}]}]}),
            json!({"content": [{"type": "text", "text": "4"}]})
        ]),
        expected
    );
}

#[test]
fn text_reasoning_text_yields_three_blocks() {
    let transcript = [
        delta(json!({"content": "a"})),
        delta(json!({"reasoning_content": "b"})),
        delta(json!({"content": "c"})),
        finish("stop"),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        accumulate(&decode_stream(&transcript)).parts,
        vec![Part::text("a"), Part::reasoning("b"), Part::text("c")]
    );
}

#[test]
fn encrypted_reasoning_detail_is_a_redacted_block() {
    let transcript = [
        delta(json!({"reasoning": "summary", "reasoning_details": [
            {"type": "reasoning.summary", "summary": "summary", "index": 0}
        ]})),
        delta(json!({"reasoning_details": [
            {"type": "reasoning.encrypted", "data": "gAAAAAB", "id": "rs_1", "index": 1}
        ]})),
        delta(json!({"content": "ok"})),
        finish("stop"),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        accumulate(&decode_stream(&transcript)).parts,
        vec![
            Part::reasoning("summary"),
            Part::Reasoning(Reasoning {
                id: Some("rs_1".into()),
                text: String::new(),
                signature: Some(Signature::new(Protocol::OpenaiChat, "gAAAAAB")),
                redacted: true,
            }),
            Part::text("ok"),
        ]
    );
}

#[test]
fn reasoning_after_a_signature_starts_a_new_block() {
    let detail = |text: &str, signature: Option<&str>, index: i64| {
        let mut d = json!({"type": "reasoning.text", "index": index});
        if !text.is_empty() {
            d["text"] = json!(text);
        }
        if let Some(signature) = signature {
            d["signature"] = json!(signature);
        }
        delta(json!({"reasoning_details": [d]}))
    };
    let transcript = [
        detail("first", None, 0),
        detail("", Some("sig-1"), 0),
        detail("second", None, 1),
        detail("", Some("sig-2"), 1),
        finish("stop"),
        DONE.to_string(),
    ]
    .concat();
    let signed = |text: &str, signature: &str| {
        Part::Reasoning(Reasoning {
            id: None,
            text: text.into(),
            signature: Some(Signature::new(Protocol::OpenaiChat, signature)),
            redacted: false,
        })
    };
    assert_eq!(
        accumulate(&decode_stream(&transcript)).parts,
        vec![signed("first", "sig-1"), signed("second", "sig-2")]
    );
}

#[test]
fn refusal_stream() {
    let transcript = [
        delta(json!({"role": "assistant", "content": null, "refusal": ""})),
        delta(json!({"refusal": "I can't"})),
        delta(json!({"refusal": " help with that."})),
        finish("stop"),
        DONE.to_string(),
    ]
    .concat();
    let events = decode_stream(&transcript);
    assert_eq!(
        events,
        vec![
            start(),
            block_start(0, BlockStart::Refusal),
            text(0, "I can't"),
            text(0, " help with that."),
            stop(0),
            finished(FinishReason::Stop),
        ]
    );
    assert_eq!(
        accumulate(&events).parts,
        vec![Part::Refusal(RefusalPart {
            text: "I can't help with that.".into()
        })]
    );
}

#[test]
fn annotations_and_generated_images() {
    let transcript = [
        delta(json!({"content": "Paris."})),
        delta(json!({"annotations": [{"type": "url_citation", "url_citation": {
            "url": "https://example.com/paris", "title": "Paris", "start_index": 0, "end_index": 6
        }}]})),
        delta(json!({"images": [
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}, "index": 0}
        ]})),
        finish("stop"),
        DONE.to_string(),
    ]
    .concat();
    let response = accumulate(&decode_stream(&transcript));
    assert_eq!(response.parts.len(), 2);
    match &response.parts[0] {
        Part::Text(t) => {
            assert_eq!(t.text, "Paris.");
            assert_eq!(
                t.citations,
                vec![Citation {
                    url: Some("https://example.com/paris".into()),
                    title: Some("Paris".into()),
                    cited_text: None,
                    start: Some(0),
                    end: Some(6),
                }]
            );
        }
        other => panic!("expected text, got {other:?}"),
    }
    assert_eq!(
        response.parts[1],
        Part::Image(MediaPart::base64("image/png", "AAAA"))
    );
}

// ---------------------------------------------------------------------------
// Usage placement
// ---------------------------------------------------------------------------

fn usage_json(prompt: u64, completion: u64) -> Value {
    json!({"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion})
}

fn with_usage(delta: Value, finish: Value, usage: Value) -> String {
    let chunk = json!({
        "id": "chatcmpl-T1", "object": "chat.completion.chunk", "created": 1741570002,
        "model": "gpt-4o",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        "usage": usage
    });
    format!("data: {chunk}\n\n")
}

#[test]
fn usage_on_every_chunk_is_a_running_total() {
    let transcript = [
        with_usage(json!({"content": "a"}), Value::Null, usage_json(5, 1)),
        with_usage(json!({"content": "b"}), Value::Null, usage_json(5, 2)),
        // An unchanged snapshot is not repeated.
        with_usage(json!({}), json!("stop"), usage_json(5, 2)),
        DONE.to_string(),
    ]
    .concat();
    let events = decode_stream(&transcript);
    let usages: Vec<&StreamEvent> = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::Usage(_)))
        .collect();
    assert_eq!(usages.len(), 2);
    let response = accumulate(&events);
    assert_eq!(response.text(), "ab");
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 5,
            output_tokens: 2,
            ..Usage::default()
        }
    );
}

#[test]
fn usage_inside_the_final_choice() {
    // Moonshot / Kimi.
    let transcript = format!(
        "{}data: {}\n\n{DONE}",
        delta(json!({"content": "hi"})),
        json!({
            "id": "chatcmpl-T1", "object": "chat.completion.chunk", "created": 1741570002,
            "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop",
                         "usage": {"prompt_tokens": 8, "completion_tokens": 1, "total_tokens": 9}}]
        })
    );
    assert_eq!(
        accumulate(&decode_stream(&transcript)).usage,
        Usage {
            input_tokens: 8,
            output_tokens: 1,
            ..Usage::default()
        }
    );
}

#[test]
fn no_usage_at_all() {
    let transcript = [
        delta(json!({"content": "hi"})),
        finish("stop"),
        DONE.to_string(),
    ]
    .concat();
    let events = decode_stream(&transcript);
    assert!(!events.iter().any(|e| matches!(e, StreamEvent::Usage(_))));
    assert_eq!(accumulate(&events).usage, Usage::default());
}

#[test]
fn usage_only_chunk_without_finish_reason_still_ends_the_turn() {
    let transcript = format!(
        "{}data: {}\n\n",
        delta(json!({"content": "hi"})),
        json!({"id": "chatcmpl-T1", "model": "gpt-4o", "choices": [], "usage": usage_json(3, 1)})
    );
    let response = accumulate(&decode_stream(&transcript));
    // An interrupted stream never delivers the usage chunk, so this one is
    // complete even though no finish reason and no `[DONE]` were sent.
    assert_eq!(response.finish, FinishReason::Stop);
    assert_eq!(response.usage.output_tokens, 1);
}

// ---------------------------------------------------------------------------
// Finish reasons and terminators
// ---------------------------------------------------------------------------

#[test]
fn finish_reason_variants() {
    let reason = |raw: &str| {
        let transcript = [
            delta(json!({"content": "x"})),
            finish(raw),
            DONE.to_string(),
        ]
        .concat();
        accumulate(&decode_stream(&transcript)).finish
    };
    assert_eq!(reason("stop"), FinishReason::Stop);
    assert_eq!(reason("length"), FinishReason::Length);
    assert_eq!(reason("content_filter"), FinishReason::ContentFilter);
    assert_eq!(reason("tool_calls"), FinishReason::Stop);
    assert_eq!(reason("function_call"), FinishReason::Stop);
    assert_eq!(reason("max_tokens"), FinishReason::Length);
    assert_eq!(reason("error"), FinishReason::Error);
    assert_eq!(reason("weird"), FinishReason::Other("weird".into()));
}

#[test]
fn missing_done_after_a_finish_reason_is_a_complete_stream() {
    let transcript = [delta(json!({"content": "hi"})), finish("stop")].concat();
    assert_eq!(
        decode_stream(&transcript),
        vec![
            start(),
            block_start(0, BlockStart::Text),
            text(0, "hi"),
            stop(0),
            finished(FinishReason::Stop),
        ]
    );
}

#[test]
fn done_without_a_finish_reason_infers_one() {
    let transcript = [delta(json!({"content": "hi"})), DONE.to_string()].concat();
    assert_eq!(
        accumulate(&decode_stream(&transcript)).finish,
        FinishReason::Stop
    );

    let transcript = [tool_delta(0, Some("c"), Some("f"), "{}"), DONE.to_string()].concat();
    assert_eq!(
        accumulate(&decode_stream(&transcript)).finish,
        FinishReason::ToolCalls
    );
}

#[test]
fn stop_reason_string_is_reported_as_the_stop_sequence() {
    // vLLM.
    let transcript = format!(
        "{}data: {}\n\n{DONE}",
        delta(json!({"content": "abc"})),
        json!({"id": "chatcmpl-T1", "model": "gpt-4o", "choices": [
            {"index": 0, "delta": {}, "finish_reason": "stop", "stop_reason": "###"}
        ]})
    );
    assert_eq!(
        decode_stream(&transcript).last(),
        Some(&StreamEvent::Finish {
            reason: FinishReason::Stop,
            stop_sequence: Some("###".into())
        })
    );
}

#[test]
fn nothing_is_decoded_after_done() {
    let transcript = [
        delta(json!({"content": "hi"})),
        finish("stop"),
        DONE.to_string(),
        delta(json!({"content": "late"})),
        DONE.to_string(),
    ]
    .concat();
    let events = decode_stream(&transcript);
    assert_eq!(events.len(), 5);
    assert_eq!(accumulate(&events).text(), "hi");
}

/// A terminator with nothing before it is an upstream that generated no
/// response. `decode` stays silent (so the gateway still sees an attempt
/// that never started and can retry it elsewhere) and `finish` ends the
/// stream as failed, exactly like a stream that sent nothing at all.
#[test]
fn done_alone_is_an_empty_stream_not_an_answer() {
    let mut decoder = ChatCodec.stream_decoder();
    assert_eq!(decoder.decode(&sse(DONE)[0]).unwrap(), vec![]);

    for transcript in [
        DONE.to_string(),
        String::new(),
        ": keep-alive\n\n".to_string(),
    ] {
        let events = decode_stream(&transcript);
        assert_eq!(events.len(), 2, "{transcript:?}");
        match &events[0] {
            StreamEvent::Start { id, model, created } => {
                assert!(id.starts_with("chatcmpl-"));
                assert_eq!((model.as_str(), *created), ("", 0));
            }
            other => panic!("expected Start, got {other:?}"),
        }
        assert_eq!(events[1], finished(FinishReason::Error));
    }
}

/// Once the upstream has started answering, `[DONE]` completes the stream
/// even when no finish reason was sent.
#[test]
fn done_after_a_role_chunk_completes_the_stream() {
    let transcript = [
        delta(json!({"role": "assistant", "content": ""})),
        DONE.to_string(),
    ]
    .concat();
    let events = decode_stream(&transcript);
    assert_eq!(events.len(), 2);
    assert_eq!(events[1], finished(FinishReason::Stop));
}

// ---------------------------------------------------------------------------
// Truncated streams
// ---------------------------------------------------------------------------

#[test]
fn truncated_text_stream_finishes_with_an_error_reason() {
    let transcript = [
        delta(json!({"role": "assistant", "content": ""})),
        delta(json!({"content": "par"})),
    ]
    .concat();
    assert_eq!(
        decode_stream(&transcript),
        vec![
            start(),
            block_start(0, BlockStart::Text),
            text(0, "par"),
            stop(0),
            finished(FinishReason::Error),
        ]
    );
}

#[test]
fn truncated_tool_call_is_closed_with_what_arrived() {
    let transcript = [
        tool_delta(0, Some("call_a"), Some("alpha"), "{\"x\":"),
        tool_delta(1, Some("call_b"), Some("beta"), "{\"y\""),
    ]
    .concat();
    let events = decode_stream(&transcript);
    let response = accumulate(&events);
    assert_eq!(
        response.parts,
        vec![
            Part::tool_call("call_a", "alpha", "{\"x\":"),
            Part::tool_call("call_b", "beta", "{\"y\""),
        ]
    );
    assert_eq!(response.finish, FinishReason::Error);
}

#[test]
fn stream_that_never_sent_anything() {
    let events = decode_stream("");
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0], StreamEvent::Start { .. }));
    assert_eq!(events[1], finished(FinishReason::Error));

    // Only noise.
    let events = decode_stream(": keep-alive\n\ndata: {\"type\":\"ping\"}\n\n");
    assert_eq!(events.len(), 2);
    assert_eq!(events[1], finished(FinishReason::Error));
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[test]
fn openrouter_error_mid_stream() {
    let transcript = r#"data: {"id":"gen-1","object":"chat.completion.chunk","created":1741570500,"model":"meta/llama","choices":[{"index":0,"delta":{"role":"assistant","content":"Partial"},"finish_reason":null}]}

data: {"id":"gen-1","object":"chat.completion.chunk","created":1741570500,"model":"meta/llama","provider":"Together","error":{"code":502,"message":"Provider returned error","metadata":{"raw":"upstream connect error","provider_name":"Together"}},"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":"error","native_finish_reason":"error"}]}

data: [DONE]

"#;
    let events = decode_stream(transcript);
    assert_eq!(events.len(), 5);
    assert_eq!(events[3], stop(0));
    match &events[4] {
        StreamEvent::Error(error) => {
            assert_eq!(error.kind, ErrorKind::Upstream);
            assert_eq!(error.status, 502);
            assert_eq!(
                error.message,
                "Provider returned error (upstream connect error)"
            );
            assert_eq!(error.code, None);
        }
        other => panic!("expected an error, got {other:?}"),
    }
    let response = accumulate(&events);
    assert_eq!(response.text(), "Partial");
    assert_eq!(response.finish, FinishReason::Error);
}

#[test]
fn openai_error_frame_with_retry_hint() {
    let transcript = [
        delta(json!({"content": "x"})),
        "data: {\"error\":{\"message\":\"Rate limit reached for gpt-4o. Please try again in 1.5s.\",\"type\":\"rate_limit_error\",\"param\":null,\"code\":\"rate_limit_exceeded\"}}\n\n".to_string(),
    ]
    .concat();
    let events = decode_stream(&transcript);
    match events.last() {
        Some(StreamEvent::Error(error)) => {
            assert_eq!(error.kind, ErrorKind::RateLimit);
            assert_eq!(error.status, 429);
            assert_eq!(error.code.as_deref(), Some("rate_limit_exceeded"));
            assert_eq!(error.retry_after_secs, Some(2));
        }
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn error_as_the_very_first_event() {
    let events = decode_stream(
        "data: {\"error\":{\"message\":\"The server is overloaded\",\"type\":\"server_error\",\"code\":\"server_is_overloaded\"}}\n\n",
    );
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0], StreamEvent::Start { .. }));
    match &events[1] {
        StreamEvent::Error(error) => {
            assert_eq!(error.kind, ErrorKind::Unavailable);
            assert_eq!(error.status, 503);
        }
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn named_error_events_and_string_errors() {
    let kind = |transcript: &str| match decode_stream(transcript).last() {
        Some(StreamEvent::Error(error)) => (error.kind, error.message.clone()),
        other => panic!("expected an error, got {other:?}"),
    };
    assert_eq!(
        kind("event: error\ndata: {\"message\":\"backend exploded\",\"code\":500}\n\n"),
        (ErrorKind::Upstream, "backend exploded".to_string())
    );
    assert_eq!(
        kind("event: error\ndata: connection lost\n\n"),
        (ErrorKind::Upstream, "connection lost".to_string())
    );
    assert_eq!(
        kind(
            "data: {\"error\":\"context length exceeded\",\"code\":\"context_length_exceeded\"}\n\n"
        ),
        (
            ErrorKind::InvalidRequest,
            "context length exceeded".to_string()
        )
    );
    // An upstream key problem is not the client's key problem.
    assert_eq!(
        kind(
            "data: {\"error\":{\"message\":\"Incorrect API key\",\"type\":\"authentication_error\",\"code\":\"invalid_api_key\"}}\n\n"
        ),
        (ErrorKind::Upstream, "Incorrect API key".to_string())
    );
}

#[test]
fn null_error_member_is_not_an_error() {
    let transcript = format!(
        "data: {}\n\n{}{DONE}",
        json!({"id": "chatcmpl-T1", "model": "gpt-4o", "created": 1741570002, "error": null,
               "choices": [{"index": 0, "delta": {"content": "fine"}}]}),
        finish("stop")
    );
    assert_eq!(accumulate(&decode_stream(&transcript)).text(), "fine");
}

// ---------------------------------------------------------------------------
// Noise and odd envelopes
// ---------------------------------------------------------------------------

#[test]
fn unknown_events_are_skipped() {
    let transcript = [
        ": OPENROUTER PROCESSING\n\n".to_string(),
        // Azure opens with a content-filter preamble that has no id or model.
        "data: {\"choices\":[],\"created\":0,\"id\":\"\",\"model\":\"\",\"object\":\"\",\"prompt_filter_results\":[{\"prompt_index\":0,\"content_filter_results\":{}}]}\n\n".to_string(),
        "event: ping\ndata: {\"type\":\"ping\"}\n\n".to_string(),
        delta(json!({"role": "assistant", "content": "Hel"})),
        "data: not json at all\n\n".to_string(),
        "data: {\"object\":\"chat.completion.heartbeat\"}\n\n".to_string(),
        "data: [1,2,3]\n\n".to_string(),
        "data: \n\n".to_string(),
        "event: metrics\ndata: {\"tokens_per_second\":41.5}\n\n".to_string(),
        delta(json!({"content": "lo"})),
        // Another choice of an `n > 1` request.
        "data: {\"id\":\"chatcmpl-T1\",\"choices\":[{\"index\":1,\"delta\":{\"content\":\"other\"}}]}\n\n".to_string(),
        finish("stop"),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        decode_stream(&transcript),
        vec![
            // Id, model and time come from the first real chunk.
            start(),
            block_start(0, BlockStart::Text),
            text(0, "Hel"),
            text(0, "lo"),
            stop(0),
            finished(FinishReason::Stop),
        ]
    );
}

#[test]
fn chunks_without_ids_get_a_minted_response_id() {
    let transcript = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let events = decode_stream(transcript);
    match &events[0] {
        StreamEvent::Start { id, .. } => assert!(id.starts_with("chatcmpl-") && id.len() == 33),
        other => panic!("expected Start, got {other:?}"),
    }
    assert_eq!(accumulate(&events).text(), "hi");
}

#[test]
fn legacy_completion_chunks_are_text() {
    let transcript = "data: {\"id\":\"cmpl-1\",\"object\":\"text_completion\",\"model\":\"m\",\"choices\":[{\"index\":0,\"text\":\"once upon\",\"finish_reason\":null}]}\n\ndata: {\"id\":\"cmpl-1\",\"object\":\"text_completion\",\"model\":\"m\",\"choices\":[{\"index\":0,\"text\":\" a time\",\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n";
    let response = accumulate(&decode_stream(transcript));
    assert_eq!(response.text(), "once upon a time");
    assert_eq!(response.finish, FinishReason::Length);
}

#[test]
fn audio_transcript_deltas_are_text() {
    let transcript = [
        delta(json!({"role": "assistant", "content": null})),
        delta(json!({"audio": {"id": "audio_1", "transcript": "Hello"}})),
        delta(json!({"audio": {"transcript": " there"}})),
        delta(json!({"audio": {"data": "UklGRg=="}})),
        finish("stop"),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        accumulate(&decode_stream(&transcript)).text(),
        "Hello there"
    );
}

#[test]
fn a_changing_id_without_a_name_is_still_the_same_call() {
    let transcript = [
        tool_delta(0, Some("call_a"), Some("alpha"), "{\"x\""),
        // No name: a fragment, whatever the id says.
        tool_delta(0, Some("chunk-2"), None, ":1}"),
        finish("tool_calls"),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        accumulate(&decode_stream(&transcript)).parts,
        vec![Part::tool_call("call_a", "alpha", "{\"x\":1}")]
    );
}

#[test]
fn fragments_after_a_call_was_closed_are_ignored() {
    let transcript = [
        tool_delta(0, Some("call_a"), Some("alpha"), "{\"x\":1}"),
        tool_delta(1, Some("call_b"), Some("beta"), "{}"),
        // The first call was complete JSON and has been closed.
        tool_delta(0, None, None, " trailing garbage"),
        finish("tool_calls"),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        accumulate(&decode_stream(&transcript)).parts,
        vec![
            Part::tool_call("call_a", "alpha", "{\"x\":1}"),
            Part::tool_call("call_b", "beta", "{}"),
        ]
    );
}

#[test]
fn byte_split_transcript_decodes_identically() {
    // The SSE parser hands over whole events however the bytes arrive.
    let whole = decode_stream(OPENAI_TOOLS);
    for cut in [1, 7, 100, 512, OPENAI_TOOLS.len() - 3] {
        let mut parser = switchyard_core::sse::SseParser::new();
        let mut events = parser
            .push(&OPENAI_TOOLS.as_bytes()[..cut])
            .expect("within limits");
        events.extend(
            parser
                .push(&OPENAI_TOOLS.as_bytes()[cut..])
                .expect("within limits"),
        );
        events.extend(parser.finish());
        assert_eq!(common::decode_events(&events), whole, "cut at {cut}");
    }
}
