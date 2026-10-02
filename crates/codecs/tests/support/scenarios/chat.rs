//! OpenAI Chat Completions: requests as the `openai` SDKs write them, and
//! responses as OpenAI and its compatible servers answer.

use super::{
    ClientRequest, DOTTED_TOOL, Expect, IMAGE_URL, LONG_TOOL, MediaKind, PDF_B64, PNG_B64,
    StreamEnd, TOOL_PNG_B64, UpstreamResponse, WEATHER_QUESTION,
};
use serde_json::{Value, json};
use switchyard_core::ir::FinishReason;
use switchyard_core::reasoning::{Depth, Effort};

const MODEL: &str = "gpt-5.5";

fn weather_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get the current weather for a city.",
            "parameters": {
                "type": "object",
                "properties": {
                    "location": {"type": "string", "description": "City name"},
                    "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
                },
                "required": ["location"],
                "additionalProperties": false
            }
        }
    })
}

fn search_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "search_docs",
            "description": "Search the documentation.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1}
                },
                "required": ["query"]
            }
        }
    })
}

/// The `ClientCtx::request` of the response-side matrix.
pub fn tool_request() -> Value {
    json!({
        "model": MODEL,
        "messages": [{"role": "user", "content": WEATHER_QUESTION}],
        "tools": [weather_tool(), search_tool()],
        "stream": true,
        "stream_options": {"include_usage": true}
    })
}

pub fn requests() -> Vec<ClientRequest> {
    vec![
        ClientRequest::new(
            "plain",
            json!({
                "model": MODEL,
                "messages": [{"role": "user", "content": "What is the capital of France?"}]
            }),
        )
        .texts(&["What is the capital of France?"]),
        ClientRequest::new(
            "system",
            json!({
                "model": MODEL,
                "messages": [
                    {"role": "system", "content": "You are a terse assistant."},
                    {"role": "developer", "content": [{"type": "text", "text": "Answer in one sentence."}]},
                    {"role": "user", "content": "Why is the sky blue?"}
                ]
            }),
        )
        .system(&["You are a terse assistant.", "Answer in one sentence."])
        .texts(&["Why is the sky blue?"]),
        ClientRequest::new(
            "multi_turn",
            json!({
                "model": MODEL,
                "messages": [
                    {"role": "user", "content": "My name is Ada."},
                    {"role": "assistant", "content": "Nice to meet you, Ada."},
                    {"role": "user", "content": "What is my name?"}
                ],
                "stream": true,
                "stream_options": {"include_usage": true}
            }),
        )
        .texts(&["My name is Ada.", "Nice to meet you, Ada.", "What is my name?"])
        .streaming(),
        ClientRequest::new(
            "image_url",
            json!({
                "model": MODEL,
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "What is in this picture?"},
                    {"type": "image_url", "image_url": {"url": IMAGE_URL, "detail": "high"}}
                ]}]
            }),
        )
        .texts(&["What is in this picture?"])
        .media(MediaKind::ImageUrl, IMAGE_URL),
        ClientRequest::new(
            "image_base64",
            json!({
                "model": MODEL,
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "Describe this image."},
                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{PNG_B64}")}}
                ]}]
            }),
        )
        .texts(&["Describe this image."])
        .media(MediaKind::ImageBase64, PNG_B64),
        ClientRequest::new(
            "pdf",
            json!({
                "model": MODEL,
                "messages": [{"role": "user", "content": [
                    {"type": "file", "file": {
                        "filename": "report.pdf",
                        "file_data": format!("data:application/pdf;base64,{PDF_B64}")
                    }},
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
                "messages": [{"role": "user", "content": WEATHER_QUESTION}],
                "tools": [weather_tool(), search_tool()],
                "tool_choice": "auto",
                "parallel_tool_calls": true
            }),
        )
        .texts(&[WEATHER_QUESTION])
        .tools(&["get_weather", "search_docs"]),
        ClientRequest::new(
            "tool_forced",
            json!({
                "model": MODEL,
                "messages": [{"role": "user", "content": "Look up the weather in Oslo."}],
                "tools": [weather_tool(), search_tool()],
                "tool_choice": {"type": "function", "function": {"name": "get_weather"}},
                "reasoning_effort": "medium"
            }),
        )
        .texts(&["Look up the weather in Oslo."])
        .tools(&["get_weather", "search_docs"])
        .depth(Depth::Level(Effort::Medium)),
        ClientRequest::new(
            "tool_required",
            json!({
                "model": MODEL,
                "messages": [{"role": "user", "content": "Use a tool to answer: weather in Lima?"}],
                "tools": [weather_tool()],
                "tool_choice": "required",
                "parallel_tool_calls": false
            }),
        )
        .texts(&["Use a tool to answer: weather in Lima?"])
        .tools(&["get_weather"]),
        ClientRequest::new(
            "tool_loop",
            json!({
                "model": MODEL,
                "messages": [
                    {"role": "system", "content": "You are a weather bot."},
                    {"role": "user", "content": WEATHER_QUESTION},
                    {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "call_Ab12Cd34Ef56Gh78Ij90Kl12", "type": "function",
                         "function": {"name": "get_weather", "arguments": "{\"location\":\"Paris\",\"unit\":\"celsius\"}"}},
                        {"id": "call_Mn34Op56Qr78St90Uv12Wx34", "type": "function",
                         "function": {"name": "get_weather", "arguments": "{\"location\": \"Tokyo\"}"}}
                    ]},
                    {"role": "tool", "tool_call_id": "call_Ab12Cd34Ef56Gh78Ij90Kl12", "content": "15 degrees and cloudy"},
                    {"role": "tool", "tool_call_id": "call_Mn34Op56Qr78St90Uv12Wx34", "content": "22 degrees and sunny"},
                    {"role": "assistant", "content": "Paris is cloudy at 15, Tokyo sunny at 22."},
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
        .tools(&["get_weather"]),
        ClientRequest::new(
            "tool_error",
            json!({
                "model": MODEL,
                "messages": [
                    {"role": "user", "content": "Search the docs for the retry policy."},
                    {"role": "assistant", "content": "", "tool_calls": [
                        {"id": "call_err000000000000000000001", "type": "function",
                         "function": {"name": "search_docs", "arguments": "{\"query\":\"retry policy\"}"}}
                    ]},
                    {"role": "tool", "tool_call_id": "call_err000000000000000000001",
                     "content": "Error: index unavailable (ETIMEDOUT)"}
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
        // Chat tool messages are text-only; SDK users who return an image
        // from a tool attach it to a follow-up user message.
        ClientRequest::new(
            "tool_image",
            json!({
                "model": MODEL,
                "messages": [
                    {"role": "user", "content": "Take a screenshot of the page."},
                    {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "call_shot00000000000000000001", "type": "function",
                         "function": {"name": "screenshot", "arguments": "{}"}}
                    ]},
                    {"role": "tool", "tool_call_id": "call_shot00000000000000000001", "content": [
                        {"type": "text", "text": "Screenshot captured"}
                    ]},
                    {"role": "user", "content": [
                        {"type": "text", "text": "Here is the screenshot the tool produced."},
                        {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{TOOL_PNG_B64}")}}
                    ]}
                ],
                "tools": [{"type": "function", "function": {"name": "screenshot", "description": "Capture the page."}}]
            }),
        )
        .texts(&["Take a screenshot of the page.", "Screenshot captured"])
        .tools(&["screenshot"])
        .media(MediaKind::ToolImage, TOOL_PNG_B64),
        ClientRequest::new(
            "reasoning_effort",
            json!({
                "model": MODEL,
                "messages": [{"role": "user", "content": "Prove that the square root of 2 is irrational."}],
                "reasoning_effort": "high",
                "max_completion_tokens": 4096
            }),
        )
        .texts(&["Prove that the square root of 2 is irrational."])
        .depth(Depth::Level(Effort::High)),
        ClientRequest::new(
            "reasoning_none",
            json!({
                "model": MODEL,
                "messages": [{"role": "user", "content": "Reply with the single word ok."}],
                "reasoning_effort": "none"
            }),
        )
        .texts(&["Reply with the single word ok."])
        .depth(Depth::Off),
        ClientRequest::new(
            "json_object",
            json!({
                "model": MODEL,
                "messages": [
                    {"role": "system", "content": "Reply in JSON."},
                    {"role": "user", "content": "List three primary colours."}
                ],
                "response_format": {"type": "json_object"}
            }),
        )
        .system(&["Reply in JSON."])
        .texts(&["List three primary colours."]),
        ClientRequest::new(
            "json_schema",
            json!({
                "model": MODEL,
                "messages": [{"role": "user", "content": "Extract: Ada Lovelace, born 1815."}],
                "response_format": {"type": "json_schema", "json_schema": {
                    "name": "person",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "born": {"type": "integer"}
                        },
                        "required": ["name", "born"],
                        "additionalProperties": false
                    }
                }}
            }),
        )
        .texts(&["Extract: Ada Lovelace, born 1815."]),
        ClientRequest::new(
            "sampling",
            json!({
                "model": MODEL,
                "messages": [{"role": "user", "content": "Write a haiku about rain."}],
                "temperature": 1.5,
                "top_p": 0.9,
                "max_tokens": 256,
                "seed": 42,
                "presence_penalty": 0.5,
                "frequency_penalty": 0.25,
                "n": 1,
                "user": "user-1234"
            }),
        )
        .texts(&["Write a haiku about rain."]),
        ClientRequest::new(
            "stop",
            json!({
                "model": MODEL,
                "messages": [{"role": "user", "content": "Count from one to ten."}],
                "stop": ["seven", "STOP"],
                "max_completion_tokens": 100
            }),
        )
        .texts(&["Count from one to ten."]),
        ClientRequest::new(
            "long_conversation",
            json!({
                "model": MODEL,
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
        ClientRequest::new(
            "mid_system",
            json!({
                "model": MODEL,
                "messages": [
                    {"role": "system", "content": "You are a helpful assistant."},
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
                "messages": [
                    {"role": "system", "content": ""},
                    {"role": "user", "content": ""},
                    {"role": "assistant", "content": ""},
                    {"role": "user", "content": [{"type": "text", "text": "   "}]},
                    {"role": "assistant", "content": "\n\n", "tool_calls": [
                        {"id": "call_empty0000000000000000001", "type": "function",
                         "function": {"name": "get_weather", "arguments": ""}}
                    ]},
                    {"role": "tool", "tool_call_id": "call_empty0000000000000000001", "content": ""},
                    {"role": "user", "content": [
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
                "messages": [
                    {"role": "system", "content": "Réponds en français. 请用中文回答。"},
                    {"role": "user", "content": "Привет, 世界! 🌍 — ça va? ¿Qué tal? 😀👍🏽 مرحبا"}
                ]
            }),
        )
        .system(&["Réponds en français. 请用中文回答。"])
        .texts(&["Привет, 世界! 🌍 — ça va? ¿Qué tal? 😀👍🏽 مرحبا"]),
        // Names an OpenAI model would refuse but Chat-compatible servers
        // accept; MCP bridges produce both shapes.
        ClientRequest::new(
            "odd_tool_names",
            json!({
                "model": MODEL,
                "messages": [
                    {"role": "user", "content": "Read the notes file."},
                    {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "call_odd00000000000000000001", "type": "function",
                         "function": {"name": DOTTED_TOOL, "arguments": "{\"path\":\"/tmp/notes.txt\"}"}}
                    ]},
                    {"role": "tool", "tool_call_id": "call_odd00000000000000000001", "content": "remember the milk"},
                    {"role": "user", "content": "Now read it with the other tool."}
                ],
                "tools": [
                    {"type": "function", "function": {"name": DOTTED_TOOL, "description": "Read a file.",
                        "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}},
                    {"type": "function", "function": {"name": LONG_TOOL, "description": "Read a file, verbosely.",
                        "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}}
                ],
                "tool_choice": {"type": "function", "function": {"name": LONG_TOOL}}
            }),
        )
        .texts(&[
            "Read the notes file.",
            "/tmp/notes.txt",
            "remember the milk",
            "Now read it with the other tool.",
        ])
        .tools(&[DOTTED_TOOL, LONG_TOOL]),
        // `allowed_tools`: three tools declared, the model restricted to two
        // of them. `tools` names the ones that may still be offered.
        ClientRequest::new(
            "allowed_tools",
            json!({
                "model": MODEL,
                "messages": [{"role": "user", "content": "Weather in Oslo, or look it up in the docs."}],
                "tools": [weather_tool(), search_tool(),
                    {"type": "function", "function": {"name": "delete_account",
                        "description": "Deletes the account.",
                        "parameters": {"type": "object", "properties": {}}}}],
                "tool_choice": {"type": "allowed_tools", "allowed_tools": {"mode": "required", "tools": [
                    {"type": "function", "function": {"name": "get_weather"}},
                    {"type": "function", "function": {"name": "search_docs"}}
                ]}}
            }),
        )
        .texts(&["Weather in Oslo, or look it up in the docs."])
        .tools(&["get_weather", "search_docs"]),
        // An assistant prefill: the conversation ends with the beginning of
        // the answer the client wants continued.
        ClientRequest::new(
            "prefill",
            json!({
                "model": MODEL,
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

const TEXT_SSE: &str = r#"data: {"id":"chatcmpl-text01","object":"chat.completion.chunk","created":1759400000,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"role":"assistant","content":"","refusal":null},"logprobs":null,"finish_reason":null}],"usage":null}

data: {"id":"chatcmpl-text01","object":"chat.completion.chunk","created":1759400000,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"content":"The capital of France"},"logprobs":null,"finish_reason":null}],"usage":null}

data: {"id":"chatcmpl-text01","object":"chat.completion.chunk","created":1759400000,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"content":" is Paris."},"logprobs":null,"finish_reason":null}],"usage":null}

data: {"id":"chatcmpl-text01","object":"chat.completion.chunk","created":1759400000,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{},"logprobs":null,"finish_reason":"stop"}],"usage":null}

data: {"id":"chatcmpl-text01","object":"chat.completion.chunk","created":1759400000,"model":"gpt-5.5-2026-04-14","choices":[],"usage":{"prompt_tokens":14,"completion_tokens":7,"total_tokens":21}}

data: [DONE]

"#;

// Cache and reasoning counts in OpenAI's inclusive convention: 1000 prompt
// tokens of which 800 were read from cache and 150 written to it (the test
// vector of notes 06 §8.2), 120 completion tokens of which 64 were reasoning.
const USAGE_SSE: &str = r#"data: {"id":"chatcmpl-usage01","object":"chat.completion.chunk","created":1759400001,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}],"usage":null}

data: {"id":"chatcmpl-usage01","object":"chat.completion.chunk","created":1759400001,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"content":"Cached answer."},"finish_reason":null}],"usage":null}

data: {"id":"chatcmpl-usage01","object":"chat.completion.chunk","created":1759400001,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":null}

data: {"id":"chatcmpl-usage01","object":"chat.completion.chunk","created":1759400001,"model":"gpt-5.5-2026-04-14","choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":120,"total_tokens":1120,"prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":150,"audio_tokens":0},"completion_tokens_details":{"reasoning_tokens":64,"audio_tokens":0,"accepted_prediction_tokens":0,"rejected_prediction_tokens":0}}}

data: [DONE]

"#;

/// Signature a Chat-compatible relay (OpenRouter style) attaches to
/// reasoning in `reasoning_details`.
pub const REASONING_SIGNATURE: &str = "ChatSigErUBCkYICBgCIkDq7Zk1x0Yq2mVh3nTt9c";

const REASONING_SSE: &str = r#"data: {"id":"chatcmpl-reason01","object":"chat.completion.chunk","created":1759400002,"model":"deepseek-reasoner","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}

data: {"id":"chatcmpl-reason01","object":"chat.completion.chunk","created":1759400002,"model":"deepseek-reasoner","choices":[{"index":0,"delta":{"reasoning_content":"Assume it is rational, ","reasoning_details":[{"type":"reasoning.text","text":"Assume it is rational, ","index":0}]},"finish_reason":null}]}

data: {"id":"chatcmpl-reason01","object":"chat.completion.chunk","created":1759400002,"model":"deepseek-reasoner","choices":[{"index":0,"delta":{"reasoning_content":"then derive a contradiction.","reasoning_details":[{"type":"reasoning.text","text":"then derive a contradiction.","index":0}]},"finish_reason":null}]}

data: {"id":"chatcmpl-reason01","object":"chat.completion.chunk","created":1759400002,"model":"deepseek-reasoner","choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.text","signature":"ChatSigErUBCkYICBgCIkDq7Zk1x0Yq2mVh3nTt9c","index":0}]},"finish_reason":null}]}

data: {"id":"chatcmpl-reason01","object":"chat.completion.chunk","created":1759400002,"model":"deepseek-reasoner","choices":[{"index":0,"delta":{"content":"It is irrational: "},"finish_reason":null}]}

data: {"id":"chatcmpl-reason01","object":"chat.completion.chunk","created":1759400002,"model":"deepseek-reasoner","choices":[{"index":0,"delta":{"content":"no fraction squares to 2."},"finish_reason":null}]}

data: {"id":"chatcmpl-reason01","object":"chat.completion.chunk","created":1759400002,"model":"deepseek-reasoner","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: {"id":"chatcmpl-reason01","object":"chat.completion.chunk","created":1759400002,"model":"deepseek-reasoner","choices":[],"usage":{"prompt_tokens":20,"completion_tokens":50,"total_tokens":70,"completion_tokens_details":{"reasoning_tokens":30}}}

data: [DONE]

"#;

const TOOL_SSE: &str = r#"data: {"id":"chatcmpl-tool01","object":"chat.completion.chunk","created":1759400003,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"role":"assistant","content":null,"tool_calls":[{"index":0,"id":"call_T1aB2cD3eF4gH5iJ6kL7mN8o","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-tool01","object":"chat.completion.chunk","created":1759400003,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"location\":"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-tool01","object":"chat.completion.chunk","created":1759400003,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-tool01","object":"chat.completion.chunk","created":1759400003,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: {"id":"chatcmpl-tool01","object":"chat.completion.chunk","created":1759400003,"model":"gpt-5.5-2026-04-14","choices":[],"usage":{"prompt_tokens":60,"completion_tokens":18,"total_tokens":78}}

data: [DONE]

"#;

// Two calls whose argument fragments interleave, as OpenAI streams them.
const PARALLEL_SSE: &str = r#"data: {"id":"chatcmpl-par01","object":"chat.completion.chunk","created":1759400004,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"role":"assistant","content":null},"finish_reason":null}]}

data: {"id":"chatcmpl-par01","object":"chat.completion.chunk","created":1759400004,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_P1aaaaaaaaaaaaaaaaaaaaaa","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-par01","object":"chat.completion.chunk","created":1759400004,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"loca"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-par01","object":"chat.completion.chunk","created":1759400004,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_P2bbbbbbbbbbbbbbbbbbbbbb","type":"function","function":{"name":"search_docs","arguments":"{\"qu"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-par01","object":"chat.completion.chunk","created":1759400004,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"tion\":\"Tokyo\"}"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-par01","object":"chat.completion.chunk","created":1759400004,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"ery\":\"tokyo climate\",\"limit\":3}"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-par01","object":"chat.completion.chunk","created":1759400004,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: {"id":"chatcmpl-par01","object":"chat.completion.chunk","created":1759400004,"model":"gpt-5.5-2026-04-14","choices":[],"usage":{"prompt_tokens":70,"completion_tokens":40,"total_tokens":110}}

data: [DONE]

"#;

const REASONING_TOOL_SSE: &str = r#"data: {"id":"chatcmpl-rt01","object":"chat.completion.chunk","created":1759400005,"model":"anthropic/claude-sonnet-4.5","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}

data: {"id":"chatcmpl-rt01","object":"chat.completion.chunk","created":1759400005,"model":"anthropic/claude-sonnet-4.5","choices":[{"index":0,"delta":{"reasoning_content":"I need the weather for Paris.","reasoning_details":[{"type":"reasoning.text","text":"I need the weather for Paris.","index":0}]},"finish_reason":null}]}

data: {"id":"chatcmpl-rt01","object":"chat.completion.chunk","created":1759400005,"model":"anthropic/claude-sonnet-4.5","choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.text","signature":"ChatSigErUBCkYICBgCIkDq7Zk1x0Yq2mVh3nTt9c","index":0}]},"finish_reason":null}]}

data: {"id":"chatcmpl-rt01","object":"chat.completion.chunk","created":1759400005,"model":"anthropic/claude-sonnet-4.5","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_RT1cccccccccccccccccccc","type":"function","function":{"name":"get_weather","arguments":"{\"location\":\"Paris\"}"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-rt01","object":"chat.completion.chunk","created":1759400005,"model":"anthropic/claude-sonnet-4.5","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: {"id":"chatcmpl-rt01","object":"chat.completion.chunk","created":1759400005,"model":"anthropic/claude-sonnet-4.5","choices":[],"usage":{"prompt_tokens":80,"completion_tokens":45,"total_tokens":125,"completion_tokens_details":{"reasoning_tokens":20}}}

data: [DONE]

"#;

/// Thought signature Google's OpenAI-compatible endpoint attaches to a tool
/// call (notes 15 §6.5).
pub const TOOL_CALL_SIGNATURE: &str = "CiQBjz1rXGdvb2dsZS1jb21wYXQtdG9vbC1jYWxsLXNpZ25hdHVyZTM=";

const SIGNED_TOOL_SSE: &str = r#"data: {"id":"chatcmpl-st01","object":"chat.completion.chunk","created":1759400006,"model":"gemini-3-flash","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"function-call-1","type":"function","function":{"name":"get_weather","arguments":"{\"location\":\"Paris\"}"},"extra_content":{"google":{"thought_signature":"CiQBjz1rXGdvb2dsZS1jb21wYXQtdG9vbC1jYWxsLXNpZ25hdHVyZTM="}}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-st01","object":"chat.completion.chunk","created":1759400006,"model":"gemini-3-flash","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: {"id":"chatcmpl-st01","object":"chat.completion.chunk","created":1759400006,"model":"gemini-3-flash","choices":[],"usage":{"prompt_tokens":33,"completion_tokens":12,"total_tokens":45}}

data: [DONE]

"#;

const LENGTH_SSE: &str = r#"data: {"id":"chatcmpl-len01","object":"chat.completion.chunk","created":1759400007,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}

data: {"id":"chatcmpl-len01","object":"chat.completion.chunk","created":1759400007,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"content":"One, two, three, four,"},"finish_reason":null}]}

data: {"id":"chatcmpl-len01","object":"chat.completion.chunk","created":1759400007,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{},"finish_reason":"length"}]}

data: {"id":"chatcmpl-len01","object":"chat.completion.chunk","created":1759400007,"model":"gpt-5.5-2026-04-14","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":8,"total_tokens":20}}

data: [DONE]

"#;

const FILTER_SSE: &str = r#"data: {"id":"chatcmpl-cf01","object":"chat.completion.chunk","created":1759400008,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}

data: {"id":"chatcmpl-cf01","object":"chat.completion.chunk","created":1759400008,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"content":"Here is how"},"finish_reason":null}]}

data: {"id":"chatcmpl-cf01","object":"chat.completion.chunk","created":1759400008,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]}

data: {"id":"chatcmpl-cf01","object":"chat.completion.chunk","created":1759400008,"model":"gpt-5.5-2026-04-14","choices":[],"usage":{"prompt_tokens":15,"completion_tokens":3,"total_tokens":18}}

data: [DONE]

"#;

const REFUSAL_SSE: &str = r#"data: {"id":"chatcmpl-ref01","object":"chat.completion.chunk","created":1759400009,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"role":"assistant","content":null,"refusal":""},"finish_reason":null}]}

data: {"id":"chatcmpl-ref01","object":"chat.completion.chunk","created":1759400009,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"refusal":"I cannot help"},"finish_reason":null}]}

data: {"id":"chatcmpl-ref01","object":"chat.completion.chunk","created":1759400009,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"refusal":" with that request."},"finish_reason":null}]}

data: {"id":"chatcmpl-ref01","object":"chat.completion.chunk","created":1759400009,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: {"id":"chatcmpl-ref01","object":"chat.completion.chunk","created":1759400009,"model":"gpt-5.5-2026-04-14","choices":[],"usage":{"prompt_tokens":15,"completion_tokens":9,"total_tokens":24}}

data: [DONE]

"#;

// An error frame after HTTP 200 and some output, as compatible servers send.
const ERROR_SSE: &str = r#"data: {"id":"chatcmpl-err01","object":"chat.completion.chunk","created":1759400010,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}

data: {"id":"chatcmpl-err01","object":"chat.completion.chunk","created":1759400010,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"content":"Partial answer"},"finish_reason":null}]}

data: {"error":{"message":"The server had an error while processing your request.","type":"server_error","param":null,"code":"server_error"}}

"#;

// The connection drops: no finish reason, no usage, no [DONE].
const TRUNCATED_SSE: &str = r#"data: {"id":"chatcmpl-cut01","object":"chat.completion.chunk","created":1759400011,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}

data: {"id":"chatcmpl-cut01","object":"chat.completion.chunk","created":1759400011,"model":"gpt-5.5-2026-04-14","choices":[{"index":0,"delta":{"content":"The answer is fo"},"finish_reason":null}]}

"#;

fn completion(
    id: &str,
    model: &str,
    created: i64,
    message: Value,
    finish: &str,
    usage: Value,
) -> Value {
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "message": message, "logprobs": null, "finish_reason": finish}],
        "usage": usage,
        "service_tier": "default",
        "system_fingerprint": "fp_0123456789"
    })
}

pub fn responses() -> Vec<UpstreamResponse> {
    let openai = "gpt-5.5-2026-04-14";
    vec![
        UpstreamResponse::new(
            "text",
            completion(
                "chatcmpl-text01",
                openai,
                1759400000,
                json!({"role": "assistant", "content": "The capital of France is Paris.", "refusal": null, "annotations": []}),
                "stop",
                json!({"prompt_tokens": 14, "completion_tokens": 7, "total_tokens": 21}),
            ),
            TEXT_SSE,
            Expect::text("The capital of France is Paris.").usage(14, 0, 0, 7, 0),
        ),
        UpstreamResponse::new(
            "text_usage",
            completion(
                "chatcmpl-usage01",
                openai,
                1759400001,
                json!({"role": "assistant", "content": "Cached answer.", "refusal": null}),
                "stop",
                json!({
                    "prompt_tokens": 1000, "completion_tokens": 120, "total_tokens": 1120,
                    "prompt_tokens_details": {"cached_tokens": 800, "cache_write_tokens": 150, "audio_tokens": 0},
                    "completion_tokens_details": {"reasoning_tokens": 64, "audio_tokens": 0,
                        "accepted_prediction_tokens": 0, "rejected_prediction_tokens": 0}
                }),
            ),
            USAGE_SSE,
            // prompt 1000 − cached 800 − written 150 = 50 uncached (06 §8.2).
            Expect::text("Cached answer.").usage(50, 800, 150, 120, 64),
        ),
        UpstreamResponse::new(
            "reasoning_text",
            completion(
                "chatcmpl-reason01",
                "deepseek-reasoner",
                1759400002,
                json!({
                    "role": "assistant",
                    "content": "It is irrational: no fraction squares to 2.",
                    "reasoning_content": "Assume it is rational, then derive a contradiction.",
                    "reasoning_details": [{
                        "type": "reasoning.text",
                        "text": "Assume it is rational, then derive a contradiction.",
                        "signature": REASONING_SIGNATURE,
                        "index": 0
                    }]
                }),
                "stop",
                json!({"prompt_tokens": 20, "completion_tokens": 50, "total_tokens": 70,
                       "completion_tokens_details": {"reasoning_tokens": 30}}),
            ),
            REASONING_SSE,
            Expect::text("It is irrational: no fraction squares to 2.")
                .reasoning("Assume it is rational, then derive a contradiction.")
                .usage(20, 0, 0, 50, 30)
                .blob(REASONING_SIGNATURE),
        ),
        UpstreamResponse::new(
            "tool_call",
            completion(
                "chatcmpl-tool01",
                openai,
                1759400003,
                json!({"role": "assistant", "content": null, "refusal": null, "tool_calls": [{
                    "id": "call_T1aB2cD3eF4gH5iJ6kL7mN8o", "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"location\":\"Paris\"}"}
                }]}),
                "tool_calls",
                json!({"prompt_tokens": 60, "completion_tokens": 18, "total_tokens": 78}),
            ),
            TOOL_SSE,
            Expect::text("")
                .call("get_weather", json!({"location": "Paris"}))
                .usage(60, 0, 0, 18, 0),
        ),
        UpstreamResponse::new(
            "parallel_tools",
            completion(
                "chatcmpl-par01",
                openai,
                1759400004,
                json!({"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_P1aaaaaaaaaaaaaaaaaaaaaa", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"location\":\"Tokyo\"}"}},
                    {"id": "call_P2bbbbbbbbbbbbbbbbbbbbbb", "type": "function",
                     "function": {"name": "search_docs", "arguments": "{\"query\":\"tokyo climate\",\"limit\":3}"}}
                ]}),
                "tool_calls",
                json!({"prompt_tokens": 70, "completion_tokens": 40, "total_tokens": 110}),
            ),
            PARALLEL_SSE,
            Expect::text("")
                .call("get_weather", json!({"location": "Tokyo"}))
                .call("search_docs", json!({"query": "tokyo climate", "limit": 3}))
                .usage(70, 0, 0, 40, 0),
        ),
        // The signature sits where a Chat relay puts it: on the reasoning
        // detail, ahead of the call.
        UpstreamResponse::new(
            "reasoning_tool",
            completion(
                "chatcmpl-rt01",
                "anthropic/claude-sonnet-4.5",
                1759400005,
                json!({
                    "role": "assistant",
                    "content": "",
                    "reasoning_content": "I need the weather for Paris.",
                    "reasoning_details": [{
                        "type": "reasoning.text",
                        "text": "I need the weather for Paris.",
                        "signature": REASONING_SIGNATURE,
                        "index": 0
                    }],
                    "tool_calls": [{
                        "id": "call_RT1cccccccccccccccccccc", "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"location\":\"Paris\"}"}
                    }]
                }),
                "tool_calls",
                json!({"prompt_tokens": 80, "completion_tokens": 45, "total_tokens": 125,
                       "completion_tokens_details": {"reasoning_tokens": 20}}),
            ),
            REASONING_TOOL_SSE,
            Expect::text("")
                .reasoning("I need the weather for Paris.")
                .call("get_weather", json!({"location": "Paris"}))
                .usage(80, 0, 0, 45, 20)
                .blob(REASONING_SIGNATURE),
        ),
        // The other native placement: Google's compatible endpoint signs the
        // tool call itself.
        UpstreamResponse::new(
            "signed_tool_call",
            completion(
                "chatcmpl-st01",
                "gemini-3-flash",
                1759400006,
                json!({"role": "assistant", "content": null, "tool_calls": [{
                    "id": "function-call-1", "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"location\":\"Paris\"}"},
                    "extra_content": {"google": {"thought_signature": TOOL_CALL_SIGNATURE}}
                }]}),
                "tool_calls",
                json!({"prompt_tokens": 33, "completion_tokens": 12, "total_tokens": 45}),
            ),
            SIGNED_TOOL_SSE,
            Expect::text("")
                .call("get_weather", json!({"location": "Paris"}))
                .usage(33, 0, 0, 12, 0)
                .blob(TOOL_CALL_SIGNATURE),
        ),
        UpstreamResponse::new(
            "length",
            completion(
                "chatcmpl-len01",
                openai,
                1759400007,
                json!({"role": "assistant", "content": "One, two, three, four,", "refusal": null}),
                "length",
                json!({"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20}),
            ),
            LENGTH_SSE,
            Expect::text("One, two, three, four,")
                .finish(FinishReason::Length)
                .usage(12, 0, 0, 8, 0),
        ),
        UpstreamResponse::new(
            "content_filter",
            completion(
                "chatcmpl-cf01",
                openai,
                1759400008,
                json!({"role": "assistant", "content": "Here is how", "refusal": null}),
                "content_filter",
                json!({"prompt_tokens": 15, "completion_tokens": 3, "total_tokens": 18}),
            ),
            FILTER_SSE,
            Expect::text("Here is how")
                .finish(FinishReason::ContentFilter)
                .usage(15, 0, 0, 3, 0),
        ),
        UpstreamResponse::new(
            "refusal",
            completion(
                "chatcmpl-ref01",
                openai,
                1759400009,
                json!({"role": "assistant", "content": null, "refusal": "I cannot help with that request."}),
                "stop",
                json!({"prompt_tokens": 15, "completion_tokens": 9, "total_tokens": 24}),
            ),
            REFUSAL_SSE,
            Expect::text("")
                .refusal("I cannot help with that request.")
                .usage(15, 0, 0, 9, 0),
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
