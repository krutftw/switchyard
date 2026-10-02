//! Anthropic Messages: requests as the `anthropic` SDKs and Claude Code write
//! them, and responses as the API answers.

use super::{
    ClientRequest, DOTTED_TOOL, Expect, IMAGE_URL, LONG_TOOL, MediaKind, PDF_B64, PNG_B64,
    StreamEnd, TOOL_PNG_B64, UpstreamResponse, WEATHER_QUESTION,
};
use serde_json::{Value, json};
use switchyard_core::ir::FinishReason;
use switchyard_core::reasoning::{Depth, Effort};

const MODEL: &str = "claude-sonnet-4-5";

fn weather_tool() -> Value {
    json!({
        "name": "get_weather",
        "description": "Get the current weather for a city.",
        "input_schema": {
            "type": "object",
            "properties": {
                "location": {"type": "string", "description": "City name"},
                "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
            },
            "required": ["location"]
        }
    })
}

fn search_tool() -> Value {
    json!({
        "name": "search_docs",
        "description": "Search the documentation.",
        "input_schema": {
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer"}
            },
            "required": ["query"]
        },
        "cache_control": {"type": "ephemeral"}
    })
}

/// The `ClientCtx::request` of the response-side matrix.
pub fn tool_request() -> Value {
    json!({
        "model": MODEL,
        "max_tokens": 4096,
        "messages": [{"role": "user", "content": WEATHER_QUESTION}],
        "tools": [weather_tool(), search_tool()],
        "stream": true
    })
}

pub fn requests() -> Vec<ClientRequest> {
    vec![
        ClientRequest::new(
            "plain",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "What is the capital of France?"}]
            }),
        )
        .texts(&["What is the capital of France?"]),
        ClientRequest::new(
            "system",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "system": [
                    {"type": "text", "text": "You are a terse assistant.", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "Answer in one sentence."}
                ],
                "messages": [{"role": "user", "content": [{"type": "text", "text": "Why is the sky blue?"}]}]
            }),
        )
        .system(&["You are a terse assistant.", "Answer in one sentence."])
        .texts(&["Why is the sky blue?"]),
        ClientRequest::new(
            "multi_turn",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "system": "You are friendly.",
                "messages": [
                    {"role": "user", "content": "My name is Ada."},
                    {"role": "assistant", "content": [{"type": "text", "text": "Nice to meet you, Ada."}]},
                    {"role": "user", "content": "What is my name?"}
                ],
                "stream": true
            }),
        )
        .system(&["You are friendly."])
        .texts(&["My name is Ada.", "Nice to meet you, Ada.", "What is my name?"])
        .streaming(),
        ClientRequest::new(
            "image_url",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": [
                    {"type": "image", "source": {"type": "url", "url": IMAGE_URL}},
                    {"type": "text", "text": "What is in this picture?"}
                ]}]
            }),
        )
        .texts(&["What is in this picture?"])
        .media(MediaKind::ImageUrl, IMAGE_URL),
        ClientRequest::new(
            "image_base64",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": PNG_B64}},
                    {"type": "text", "text": "Describe this image."}
                ]}]
            }),
        )
        .texts(&["Describe this image."])
        .media(MediaKind::ImageBase64, PNG_B64),
        ClientRequest::new(
            "pdf",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": [
                    {"type": "document", "title": "report.pdf",
                     "source": {"type": "base64", "media_type": "application/pdf", "data": PDF_B64},
                     "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "Summarise the attached document."}
                ]}]
            }),
        )
        .texts(&["Summarise the attached document."])
        .media(MediaKind::Pdf, PDF_B64),
        ClientRequest::new(
            "tools_auto",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": WEATHER_QUESTION}],
                "tools": [weather_tool(), search_tool()],
                "tool_choice": {"type": "auto"}
            }),
        )
        .texts(&[WEATHER_QUESTION])
        .tools(&["get_weather", "search_docs"]),
        ClientRequest::new(
            "tool_forced",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "Look up the weather in Oslo."}],
                "tools": [weather_tool(), search_tool()],
                "tool_choice": {"type": "tool", "name": "get_weather"}
            }),
        )
        .texts(&["Look up the weather in Oslo."])
        .tools(&["get_weather", "search_docs"]),
        ClientRequest::new(
            "tool_required",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "Use a tool to answer: weather in Lima?"}],
                "tools": [weather_tool()],
                "tool_choice": {"type": "any", "disable_parallel_tool_use": true}
            }),
        )
        .texts(&["Use a tool to answer: weather in Lima?"])
        .tools(&["get_weather"]),
        // A tool loop with extended thinking, as Claude Code replays it: the
        // signed thinking block leads the assistant turn.
        ClientRequest::new(
            "tool_loop",
            json!({
                "model": MODEL,
                "max_tokens": 16000,
                "system": "You are a weather bot.",
                "thinking": {"type": "enabled", "budget_tokens": 4096},
                "messages": [
                    {"role": "user", "content": WEATHER_QUESTION},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "Two cities, two calls.",
                         "signature": "EqQBCkYICBgCIkAnthropicLoopSignature0123456789abcdef"},
                        {"type": "text", "text": "Let me check both."},
                        {"type": "tool_use", "id": "toolu_01A9bC8dE7fG6hI5jK4lM3nO", "name": "get_weather",
                         "input": {"location": "Paris", "unit": "celsius"}},
                        {"type": "tool_use", "id": "toolu_01P2qR3sT4uV5wX6yZ7aB8cD", "name": "get_weather",
                         "input": {"location": "Tokyo"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_01A9bC8dE7fG6hI5jK4lM3nO",
                         "content": "15 degrees and cloudy"},
                        {"type": "tool_result", "tool_use_id": "toolu_01P2qR3sT4uV5wX6yZ7aB8cD",
                         "content": [{"type": "text", "text": "22 degrees and sunny"}]}
                    ]},
                    {"role": "assistant", "content": [
                        {"type": "text", "text": "Paris is cloudy at 15, Tokyo sunny at 22."}
                    ]},
                    {"role": "user", "content": "And what about Berlin?"}
                ],
                "tools": [weather_tool()]
            }),
        )
        .system(&["You are a weather bot."])
        .texts(&[
            WEATHER_QUESTION,
            "Paris",
            "Tokyo",
            "15 degrees and cloudy",
            "22 degrees and sunny",
            "And what about Berlin?",
        ])
        .tools(&["get_weather"])
        .depth(Depth::Budget(4096)),
        ClientRequest::new(
            "tool_error",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "Search the docs for the retry policy."},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "toolu_01Err0000000000000000001", "name": "search_docs",
                         "input": {"query": "retry policy"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_01Err0000000000000000001",
                         "content": "Error: index unavailable (ETIMEDOUT)", "is_error": true}
                    ]}
                ],
                "tools": [search_tool()]
            }),
        )
        .texts(&[
            "Search the docs for the retry policy.",
            "retry policy",
            "Error: index unavailable (ETIMEDOUT)",
        ])
        .tools(&["search_docs"]),
        ClientRequest::new(
            "tool_image",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "Take a screenshot of the page."},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "toolu_01Shot000000000000000001", "name": "screenshot", "input": {}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_01Shot000000000000000001", "content": [
                            {"type": "text", "text": "Screenshot captured"},
                            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": TOOL_PNG_B64}}
                        ]}
                    ]}
                ],
                "tools": [{"name": "screenshot", "description": "Capture the page.",
                           "input_schema": {"type": "object", "properties": {}}}]
            }),
        )
        .texts(&["Take a screenshot of the page.", "Screenshot captured"])
        .tools(&["screenshot"])
        .media(MediaKind::ToolImage, TOOL_PNG_B64),
        ClientRequest::new(
            "reasoning_budget",
            json!({
                "model": MODEL,
                "max_tokens": 16000,
                "thinking": {"type": "enabled", "budget_tokens": 10000},
                "messages": [{"role": "user", "content": "Prove that the square root of 2 is irrational."}]
            }),
        )
        .texts(&["Prove that the square root of 2 is irrational."])
        .depth(Depth::Budget(10000)),
        ClientRequest::new(
            "reasoning_adaptive",
            json!({
                "model": "claude-opus-5",
                "max_tokens": 16000,
                "thinking": {"type": "adaptive", "display": "summarized"},
                "output_config": {"effort": "high"},
                "messages": [{"role": "user", "content": "Prove that there are infinitely many primes."}]
            }),
        )
        .texts(&["Prove that there are infinitely many primes."])
        .depth(Depth::Level(Effort::High)),
        ClientRequest::new(
            "reasoning_none",
            json!({
                "model": MODEL,
                "max_tokens": 256,
                "thinking": {"type": "disabled"},
                "messages": [{"role": "user", "content": "Reply with the single word ok."}]
            }),
        )
        .texts(&["Reply with the single word ok."])
        .depth(Depth::Off),
        ClientRequest::new(
            "json_schema",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "Extract: Ada Lovelace, born 1815."}],
                "output_config": {"format": {"type": "json_schema", "schema": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "born": {"type": "integer"}
                    },
                    "required": ["name", "born"],
                    "additionalProperties": false
                }}}
            }),
        )
        .texts(&["Extract: Ada Lovelace, born 1815."]),
        ClientRequest::new(
            "sampling",
            json!({
                "model": MODEL,
                "max_tokens": 256,
                "temperature": 0.3,
                "top_p": 0.9,
                "top_k": 40,
                "metadata": {"user_id": "user-1234"},
                "service_tier": "standard_only",
                "messages": [{"role": "user", "content": "Write a haiku about rain."}]
            }),
        )
        .texts(&["Write a haiku about rain."]),
        ClientRequest::new(
            "stop",
            json!({
                "model": MODEL,
                "max_tokens": 100,
                "stop_sequences": ["seven", "STOP"],
                "messages": [{"role": "user", "content": "Count from one to ten."}]
            }),
        )
        .texts(&["Count from one to ten."]),
        // The API merges consecutive turns of one role; SDK users rely on it.
        ClientRequest::new(
            "long_conversation",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "Turn one alpha."},
                    {"role": "user", "content": "Turn two beta."},
                    {"role": "assistant", "content": "Reply three gamma."},
                    {"role": "assistant", "content": "Reply four delta."},
                    {"role": "user", "content": "Turn five epsilon."},
                    {"role": "user", "content": [{"type": "text", "text": "Turn six zeta."}]},
                    {"role": "user", "content": "Turn seven eta."},
                    {"role": "assistant", "content": "Reply eight theta."},
                    {"role": "user", "content": "Turn nine iota."},
                    {"role": "assistant", "content": "Reply ten kappa."},
                    {"role": "assistant", "content": "Reply eleven lambda."},
                    {"role": "user", "content": "Turn twelve mu."}
                ]
            }),
        )
        .texts(&[
            "Turn one alpha.",
            "Turn two beta.",
            "Reply three gamma.",
            "Reply four delta.",
            "Turn five epsilon.",
            "Turn six zeta.",
            "Turn seven eta.",
            "Reply eight theta.",
            "Turn nine iota.",
            "Reply ten kappa.",
            "Reply eleven lambda.",
            "Turn twelve mu.",
        ]),
        // Mid-conversation `system` turns, as newer clients send them.
        ClientRequest::new(
            "mid_system",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "system": "You are a helpful assistant.",
                "messages": [
                    {"role": "user", "content": "Tell me a joke."},
                    {"role": "assistant", "content": "Why did the chicken cross the road?"},
                    {"role": "system", "content": "From now on answer in French."},
                    {"role": "user", "content": "I do not know, why?"}
                ]
            }),
        )
        .system(&["You are a helpful assistant."])
        .texts(&[
            "Tell me a joke.",
            "Why did the chicken cross the road?",
            "From now on answer in French.",
            "I do not know, why?",
        ]),
        ClientRequest::new(
            "empty_contents",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "system": "",
                "messages": [
                    {"role": "user", "content": ""},
                    {"role": "assistant", "content": [{"type": "text", "text": ""}]},
                    {"role": "user", "content": [{"type": "text", "text": "   "}]},
                    {"role": "assistant", "content": [
                        {"type": "text", "text": "\n\n"},
                        {"type": "tool_use", "id": "toolu_01Empty00000000000000001", "name": "get_weather", "input": {}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_01Empty00000000000000001", "content": ""},
                        {"type": "text", "text": ""},
                        {"type": "text", "text": "Is anybody there?"}
                    ]}
                ],
                "tools": [weather_tool()]
            }),
        )
        .texts(&["Is anybody there?"])
        .tools(&["get_weather"]),
        ClientRequest::new(
            "unicode",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "system": "Réponds en français. 请用中文回答。",
                "messages": [{"role": "user", "content": "Привет, 世界! 🌍 — ça va? ¿Qué tal? 😀👍🏽 مرحبا"}]
            }),
        )
        .system(&["Réponds en français. 请用中文回答。"])
        .texts(&["Привет, 世界! 🌍 — ça va? ¿Qué tal? 😀👍🏽 مرحبا"]),
        // The long name is native (the limit is 128); the dotted one is what
        // lenient gateways in front of the API let through.
        ClientRequest::new(
            "odd_tool_names",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "Read the notes file."},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "toolu_01Odd0000000000000000001", "name": DOTTED_TOOL,
                         "input": {"path": "/tmp/notes.txt"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_01Odd0000000000000000001", "content": "remember the milk"},
                        {"type": "text", "text": "Now read it with the other tool."}
                    ]}
                ],
                "tools": [
                    {"name": DOTTED_TOOL, "description": "Read a file.",
                     "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}},
                    {"name": LONG_TOOL, "description": "Read a file, verbosely.",
                     "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}}
                ],
                "tool_choice": {"type": "tool", "name": LONG_TOOL}
            }),
        )
        .texts(&[
            "Read the notes file.",
            "/tmp/notes.txt",
            "remember the milk",
            "Now read it with the other tool.",
        ])
        .tools(&[DOTTED_TOOL, LONG_TOOL]),
        // The web-search server tool next to a function, and the server tool
        // forced by its name.
        ClientRequest::new(
            "forced_web_search",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "What changed in Rust 1.90?"}],
                "tools": [
                    {"type": "web_search_20250305", "name": "web_search", "max_uses": 3},
                    weather_tool()
                ],
                "tool_choice": {"type": "tool", "name": "web_search"}
            }),
        )
        .texts(&["What changed in Rust 1.90?"])
        .tools(&["get_weather"]),
        // An assistant prefill, as the SDK documentation shows it.
        ClientRequest::new(
            "prefill",
            json!({
                "model": MODEL,
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "Name three primary colours as a list."},
                    {"role": "assistant", "content": "Here they are: 1."}
                ]
            }),
        )
        .texts(&["Name three primary colours as a list."]),
    ]
}

// ---------------------------------------------------------------------------
// Upstream responses
// ---------------------------------------------------------------------------

/// `signature` of the thinking blocks in the canned responses.
pub const THINKING_SIGNATURE: &str = "EqQBCkYICBgCIkBAnthropicSig8f3a1c5e7b9d2f4a6c8e0b1d3f5a7c9e";

fn message(id: &str, content: Value, stop_reason: &str, usage: Value) -> Value {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-5-20250929",
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage
    })
}

const TEXT_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01Text000000000000000001","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":14,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: ping
data: {"type": "ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"The capital of France"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" is Paris."}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":7}}

event: message_stop
data: {"type":"message_stop"}

"#;

// The test vector of notes 06 §4.2: input 13, cache read 22000, cache
// creation 31, output 4 → prompt 22044 for an OpenAI client. Split across
// `message_start` and `message_delta`, as the API does.
const USAGE_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01Usage00000000000000001","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":13,"cache_creation_input_tokens":31,"cache_read_input_tokens":22000,"cache_creation":{"ephemeral_5m_input_tokens":31,"ephemeral_1h_input_tokens":0},"output_tokens":1,"service_tier":"standard"}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Cached answer."}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}

event: message_stop
data: {"type":"message_stop"}

"#;

const REASONING_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01Reason0000000000000001","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Assume it is rational, "}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"then derive a contradiction."}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCkYICBgCIkBAnthropicSig8f3a1c5e7b9d2f4a6c8e0b1d3f5a7c9e"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"It is irrational: "}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"no fraction squares to 2."}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":50,"output_tokens_details":{"thinking_tokens":30}}}

event: message_stop
data: {"type":"message_stop"}

"#;

const TOOL_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01Tool000000000000000001","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":60,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01T1aB2cD3eF4gH5iJ6kL7mN","name":"get_weather","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"location\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":" \"Paris\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":18}}

event: message_stop
data: {"type":"message_stop"}

"#;

const PARALLEL_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01Par0000000000000000001","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":70,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Checking both."}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01P1aaaaaaaaaaaaaaaaaaaa","name":"get_weather","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"loca"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"tion\": \"Tokyo\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_01P2bbbbbbbbbbbbbbbbbbbb","name":"search_docs","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"query\": \"tokyo cl"}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"imate\", \"limit\": 3}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":40}}

event: message_stop
data: {"type":"message_stop"}

"#;

const REASONING_TOOL_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01Rt00000000000000000001","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":80,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I need the weather for Paris."}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCkYICBgCIkBAnthropicSig8f3a1c5e7b9d2f4a6c8e0b1d3f5a7c9e"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01RT1ccccccccccccccccccc","name":"get_weather","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"location\": \"Paris\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":45,"output_tokens_details":{"thinking_tokens":20}}}

event: message_stop
data: {"type":"message_stop"}

"#;

const LENGTH_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01Len0000000000000000001","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":12,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"One, two, three, four,"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":8}}

event: message_stop
data: {"type":"message_stop"}

"#;

// A refusal: HTTP 200, `stop_reason: "refusal"`, partial output (notes 15 §5.4).
const REFUSAL_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01Ref0000000000000000001","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":15,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Here is how"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"refusal","stop_sequence":null},"usage":{"output_tokens":3}}

event: message_stop
data: {"type":"message_stop"}

"#;

const STOP_SEQUENCE_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01Stop000000000000000001","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":12,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"one two three four five six "}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"stop_sequence","stop_sequence":"seven"},"usage":{"output_tokens":9}}

event: message_stop
data: {"type":"message_stop"}

"#;

// The mid-stream error of notes 15 §5.5.
const ERROR_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01Err0000000000000000001","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Partial answer"}}

event: error
data: {"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}

"#;

const TRUNCATED_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01Cut0000000000000000001","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"The answer is fo"}}

"#;

fn usage(input: u64, output: u64) -> Value {
    json!({"input_tokens": input, "cache_creation_input_tokens": 0,
           "cache_read_input_tokens": 0, "output_tokens": output})
}

pub fn responses() -> Vec<UpstreamResponse> {
    vec![
        UpstreamResponse::new(
            "text",
            message(
                "msg_01Text000000000000000001",
                json!([{"type": "text", "text": "The capital of France is Paris."}]),
                "end_turn",
                usage(14, 7),
            ),
            TEXT_SSE,
            Expect::text("The capital of France is Paris.").usage(14, 0, 0, 7, 0),
        ),
        UpstreamResponse::new(
            "text_usage",
            message(
                "msg_01Usage00000000000000001",
                json!([{"type": "text", "text": "Cached answer."}]),
                "end_turn",
                json!({
                    "input_tokens": 13, "cache_creation_input_tokens": 31, "cache_read_input_tokens": 22000,
                    "cache_creation": {"ephemeral_5m_input_tokens": 31, "ephemeral_1h_input_tokens": 0},
                    "output_tokens": 4, "service_tier": "standard"
                }),
            ),
            USAGE_SSE,
            Expect::text("Cached answer.").usage(13, 22000, 31, 4, 0),
        ),
        UpstreamResponse::new(
            "reasoning_text",
            message(
                "msg_01Reason0000000000000001",
                json!([
                    {"type": "thinking", "thinking": "Assume it is rational, then derive a contradiction.",
                     "signature": THINKING_SIGNATURE},
                    {"type": "text", "text": "It is irrational: no fraction squares to 2."}
                ]),
                "end_turn",
                json!({"input_tokens": 20, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                       "output_tokens": 50, "output_tokens_details": {"thinking_tokens": 30}}),
            ),
            REASONING_SSE,
            Expect::text("It is irrational: no fraction squares to 2.")
                .reasoning("Assume it is rational, then derive a contradiction.")
                .usage(20, 0, 0, 50, 30)
                .blob(THINKING_SIGNATURE),
        ),
        UpstreamResponse::new(
            "tool_call",
            message(
                "msg_01Tool000000000000000001",
                json!([{"type": "tool_use", "id": "toolu_01T1aB2cD3eF4gH5iJ6kL7mN", "name": "get_weather",
                        "input": {"location": "Paris"}}]),
                "tool_use",
                usage(60, 18),
            ),
            TOOL_SSE,
            Expect::text("")
                .call("get_weather", json!({"location": "Paris"}))
                .usage(60, 0, 0, 18, 0),
        ),
        UpstreamResponse::new(
            "parallel_tools",
            message(
                "msg_01Par0000000000000000001",
                json!([
                    {"type": "text", "text": "Checking both."},
                    {"type": "tool_use", "id": "toolu_01P1aaaaaaaaaaaaaaaaaaaa", "name": "get_weather",
                     "input": {"location": "Tokyo"}},
                    {"type": "tool_use", "id": "toolu_01P2bbbbbbbbbbbbbbbbbbbb", "name": "search_docs",
                     "input": {"query": "tokyo climate", "limit": 3}}
                ]),
                "tool_use",
                usage(70, 40),
            ),
            PARALLEL_SSE,
            Expect::text("Checking both.")
                .call("get_weather", json!({"location": "Tokyo"}))
                .call("search_docs", json!({"query": "tokyo climate", "limit": 3}))
                .usage(70, 0, 0, 40, 0),
        ),
        // Native placement: the signed thinking block opens the turn, the
        // tool_use follows.
        UpstreamResponse::new(
            "reasoning_tool",
            message(
                "msg_01Rt00000000000000000001",
                json!([
                    {"type": "thinking", "thinking": "I need the weather for Paris.", "signature": THINKING_SIGNATURE},
                    {"type": "tool_use", "id": "toolu_01RT1ccccccccccccccccccc", "name": "get_weather",
                     "input": {"location": "Paris"}}
                ]),
                "tool_use",
                json!({"input_tokens": 80, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                       "output_tokens": 45, "output_tokens_details": {"thinking_tokens": 20}}),
            ),
            REASONING_TOOL_SSE,
            Expect::text("")
                .reasoning("I need the weather for Paris.")
                .call("get_weather", json!({"location": "Paris"}))
                .usage(80, 0, 0, 45, 20)
                .blob(THINKING_SIGNATURE),
        ),
        UpstreamResponse::new(
            "length",
            message(
                "msg_01Len0000000000000000001",
                json!([{"type": "text", "text": "One, two, three, four,"}]),
                "max_tokens",
                usage(12, 8),
            ),
            LENGTH_SSE,
            Expect::text("One, two, three, four,")
                .finish(FinishReason::Length)
                .usage(12, 0, 0, 8, 0),
        ),
        UpstreamResponse::new(
            "refusal",
            json!({
                "id": "msg_01Ref0000000000000000001",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4-5-20250929",
                "content": [{"type": "text", "text": "Here is how"}],
                "stop_reason": "refusal",
                "stop_sequence": null,
                "stop_details": {"type": "refusal", "category": "cyber", "explanation": null},
                "usage": usage(15, 3)
            }),
            REFUSAL_SSE,
            Expect::text("Here is how")
                .finish(FinishReason::Refusal)
                .usage(15, 0, 0, 3, 0),
        ),
        UpstreamResponse::new(
            "stop_sequence",
            json!({
                "id": "msg_01Stop000000000000000001",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4-5-20250929",
                "content": [{"type": "text", "text": "one two three four five six "}],
                "stop_reason": "stop_sequence",
                "stop_sequence": "seven",
                "usage": usage(12, 9)
            }),
            STOP_SEQUENCE_SSE,
            Expect::text("one two three four five six ").usage(12, 0, 0, 9, 0),
        ),
        UpstreamResponse::stream_only(
            "stream_error",
            ERROR_SSE,
            StreamEnd::Error,
            Expect::text("Partial answer").finish(FinishReason::Error),
        ),
        UpstreamResponse::stream_only(
            "truncated",
            TRUNCATED_SSE,
            StreamEnd::Truncated,
            Expect::text("The answer is fo").finish(FinishReason::Error),
        ),
    ]
}
