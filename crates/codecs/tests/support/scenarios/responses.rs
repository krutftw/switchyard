//! OpenAI Responses: requests as the `openai` SDKs and Codex write them, and
//! responses as the API answers.

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
        "name": "get_weather",
        "description": "Get the current weather for a city.",
        "parameters": {
            "type": "object",
            "properties": {
                "location": {"type": "string", "description": "City name"},
                "unit": {"type": ["string", "null"], "enum": ["celsius", "fahrenheit", null]}
            },
            "required": ["location", "unit"],
            "additionalProperties": false
        },
        "strict": true
    })
}

fn search_tool() -> Value {
    json!({
        "type": "function",
        "name": "search_docs",
        "description": "Search the documentation.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer"}
            },
            "required": ["query"]
        },
        "strict": false
    })
}

fn user(text: &str) -> Value {
    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
}

fn assistant(text: &str) -> Value {
    json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]})
}

/// The `ClientCtx::request` of the response-side matrix.
pub fn tool_request() -> Value {
    json!({
        "model": MODEL,
        "input": [user(WEATHER_QUESTION)],
        "tools": [weather_tool(), search_tool()],
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "stream": true
    })
}

pub fn requests() -> Vec<ClientRequest> {
    vec![
        // The shortest form: a string input.
        ClientRequest::new(
            "plain",
            json!({"model": MODEL, "input": "What is the capital of France?"}),
        )
        .texts(&["What is the capital of France?"]),
        ClientRequest::new(
            "system",
            json!({
                "model": MODEL,
                "instructions": "You are a terse assistant.",
                "input": [
                    {"role": "developer", "content": "Answer in one sentence."},
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
                "input": [
                    user("My name is Ada."),
                    assistant("Nice to meet you, Ada."),
                    user("What is my name?")
                ],
                "store": false,
                "stream": true
            }),
        )
        .texts(&["My name is Ada.", "Nice to meet you, Ada.", "What is my name?"])
        .streaming(),
        ClientRequest::new(
            "image_url",
            json!({
                "model": MODEL,
                "input": [{"role": "user", "content": [
                    {"type": "input_text", "text": "What is in this picture?"},
                    {"type": "input_image", "image_url": IMAGE_URL, "detail": "high"}
                ]}]
            }),
        )
        .texts(&["What is in this picture?"])
        .media(MediaKind::ImageUrl, IMAGE_URL),
        ClientRequest::new(
            "image_base64",
            json!({
                "model": MODEL,
                "input": [{"role": "user", "content": [
                    {"type": "input_text", "text": "Describe this image."},
                    {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG_B64}")}
                ]}]
            }),
        )
        .texts(&["Describe this image."])
        .media(MediaKind::ImageBase64, PNG_B64),
        ClientRequest::new(
            "pdf",
            json!({
                "model": MODEL,
                "input": [{"role": "user", "content": [
                    {"type": "input_file", "filename": "report.pdf",
                     "file_data": format!("data:application/pdf;base64,{PDF_B64}")},
                    {"type": "input_text", "text": "Summarise the attached document."}
                ]}]
            }),
        )
        .texts(&["Summarise the attached document."])
        .media(MediaKind::Pdf, PDF_B64),
        ClientRequest::new(
            "tools_auto",
            json!({
                "model": MODEL,
                "input": [user(WEATHER_QUESTION)],
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
                "input": "Look up the weather in Oslo.",
                "tools": [weather_tool(), search_tool()],
                "tool_choice": {"type": "function", "name": "get_weather"},
                "reasoning": {"effort": "medium"}
            }),
        )
        .texts(&["Look up the weather in Oslo."])
        .tools(&["get_weather", "search_docs"])
        .depth(Depth::Level(Effort::Medium)),
        ClientRequest::new(
            "tool_required",
            json!({
                "model": MODEL,
                "input": "Use a tool to answer: weather in Lima?",
                "tools": [weather_tool()],
                "tool_choice": "required",
                "parallel_tool_calls": false
            }),
        )
        .texts(&["Use a tool to answer: weather in Lima?"])
        .tools(&["get_weather"]),
        // A stateless tool loop, as Codex replays it: reasoning item with its
        // encrypted content, the calls, their outputs.
        ClientRequest::new(
            "tool_loop",
            json!({
                "model": MODEL,
                "instructions": "You are a weather bot.",
                "input": [
                    user(WEATHER_QUESTION),
                    {"type": "reasoning", "id": "rs_0a1b2c3d4e5f", "summary": [
                        {"type": "summary_text", "text": "Two cities, two calls."}
                    ], "encrypted_content": "gAAAAABoLoopBlobZXhhbXBsZS1lbmNyeXB0ZWQ="},
                    {"type": "function_call", "id": "fc_0a1b2c3d4e5f01", "call_id": "call_Ab12Cd34Ef56Gh78Ij90Kl12",
                     "name": "get_weather", "arguments": "{\"location\":\"Paris\",\"unit\":\"celsius\"}", "status": "completed"},
                    {"type": "function_call", "id": "fc_0a1b2c3d4e5f02", "call_id": "call_Mn34Op56Qr78St90Uv12Wx34",
                     "name": "get_weather", "arguments": "{\"location\":\"Tokyo\",\"unit\":null}", "status": "completed"},
                    {"type": "function_call_output", "call_id": "call_Ab12Cd34Ef56Gh78Ij90Kl12", "output": "15 degrees and cloudy"},
                    {"type": "function_call_output", "call_id": "call_Mn34Op56Qr78St90Uv12Wx34", "output": "22 degrees and sunny"},
                    assistant("Paris is cloudy at 15, Tokyo sunny at 22."),
                    user("And what about Berlin?")
                ],
                "tools": [weather_tool()],
                "store": false,
                "include": ["reasoning.encrypted_content"]
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
                "input": [
                    user("Search the docs for the retry policy."),
                    {"type": "function_call", "call_id": "call_err000000000000000000001",
                     "name": "search_docs", "arguments": "{\"query\":\"retry policy\"}"},
                    {"type": "function_call_output", "call_id": "call_err000000000000000000001",
                     "output": "Error: index unavailable (ETIMEDOUT)"}
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
                "input": [
                    user("Take a screenshot of the page."),
                    {"type": "function_call", "call_id": "call_shot00000000000000000001",
                     "name": "screenshot", "arguments": "{}"},
                    {"type": "function_call_output", "call_id": "call_shot00000000000000000001", "output": [
                        {"type": "input_text", "text": "Screenshot captured"},
                        {"type": "input_image", "image_url": format!("data:image/png;base64,{TOOL_PNG_B64}")}
                    ]}
                ],
                "tools": [{"type": "function", "name": "screenshot", "description": "Capture the page.",
                           "parameters": {"type": "object", "properties": {}}, "strict": false}]
            }),
        )
        .texts(&["Take a screenshot of the page.", "Screenshot captured"])
        .tools(&["screenshot"])
        .media(MediaKind::ToolImage, TOOL_PNG_B64),
        ClientRequest::new(
            "reasoning_effort",
            json!({
                "model": MODEL,
                "input": "Prove that the square root of 2 is irrational.",
                "reasoning": {"effort": "high", "summary": "auto"},
                "max_output_tokens": 4096,
                "store": false,
                "include": ["reasoning.encrypted_content"]
            }),
        )
        .texts(&["Prove that the square root of 2 is irrational."])
        .depth(Depth::Level(Effort::High)),
        ClientRequest::new(
            "reasoning_none",
            json!({
                "model": MODEL,
                "input": "Reply with the single word ok.",
                "reasoning": {"effort": "none"}
            }),
        )
        .texts(&["Reply with the single word ok."])
        .depth(Depth::Off),
        ClientRequest::new(
            "json_object",
            json!({
                "model": MODEL,
                "instructions": "Reply in JSON.",
                "input": "List three primary colours.",
                "text": {"format": {"type": "json_object"}}
            }),
        )
        .system(&["Reply in JSON."])
        .texts(&["List three primary colours."]),
        ClientRequest::new(
            "json_schema",
            json!({
                "model": MODEL,
                "input": "Extract: Ada Lovelace, born 1815.",
                "text": {"format": {
                    "type": "json_schema",
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
                }, "verbosity": "low"}
            }),
        )
        .texts(&["Extract: Ada Lovelace, born 1815."]),
        ClientRequest::new(
            "sampling",
            json!({
                "model": MODEL,
                "input": "Write a haiku about rain.",
                "temperature": 1.5,
                "top_p": 0.9,
                "max_output_tokens": 256,
                "user": "user-1234",
                "metadata": {"trace": "abc"},
                "service_tier": "flex"
            }),
        )
        .texts(&["Write a haiku about rain."]),
        // Responses has no stop sequences; the closest native knob is a tiny
        // output limit, below what the API itself accepts from other vendors.
        ClientRequest::new(
            "tiny_limit",
            json!({
                "model": MODEL,
                "input": "Count from one to ten.",
                "max_output_tokens": 16
            }),
        )
        .texts(&["Count from one to ten."]),
        ClientRequest::new(
            "long_conversation",
            json!({
                "model": MODEL,
                "input": [
                    user("Turn one alpha."),
                    user("Turn two beta."),
                    assistant("Reply three gamma."),
                    assistant("Reply four delta."),
                    user("Turn five epsilon."),
                    {"role": "user", "content": "Turn six zeta."},
                    user("Turn seven eta."),
                    assistant("Reply eight theta."),
                    user("Turn nine iota."),
                    assistant("Reply ten kappa."),
                    assistant("Reply eleven lambda."),
                    user("Turn twelve mu.")
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
                "instructions": "You are a helpful assistant.",
                "input": [
                    user("Tell me a joke."),
                    assistant("Why did the chicken cross the road?"),
                    {"type": "message", "role": "developer", "content": [
                        {"type": "input_text", "text": "From now on answer in French."}
                    ]},
                    user("I do not know, why?")
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
                "instructions": "",
                "input": [
                    {"role": "user", "content": ""},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": ""}]},
                    {"role": "user", "content": [{"type": "input_text", "text": "   "}]},
                    {"type": "function_call", "call_id": "call_empty0000000000000000001",
                     "name": "get_weather", "arguments": ""},
                    {"type": "function_call_output", "call_id": "call_empty0000000000000000001", "output": ""},
                    {"role": "user", "content": [
                        {"type": "input_text", "text": ""},
                        {"type": "input_text", "text": "Is anybody there?"}
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
                "instructions": "Réponds en français. 请用中文回答。",
                "input": "Привет, 世界! 🌍 — ça va? ¿Qué tal? 😀👍🏽 مرحبا"
            }),
        )
        .system(&["Réponds en français. 请用中文回答。"])
        .texts(&["Привет, 世界! 🌍 — ça va? ¿Qué tal? 😀👍🏽 مرحبا"]),
        ClientRequest::new(
            "odd_tool_names",
            json!({
                "model": MODEL,
                "input": [
                    user("Read the notes file."),
                    {"type": "function_call", "call_id": "call_odd00000000000000000001",
                     "name": DOTTED_TOOL, "arguments": "{\"path\":\"/tmp/notes.txt\"}"},
                    {"type": "function_call_output", "call_id": "call_odd00000000000000000001", "output": "remember the milk"},
                    user("Now read it with the other tool.")
                ],
                "tools": [
                    {"type": "function", "name": DOTTED_TOOL, "description": "Read a file.",
                     "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}},
                    {"type": "function", "name": LONG_TOOL, "description": "Read a file, verbosely.",
                     "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}
                ],
                "tool_choice": {"type": "function", "name": LONG_TOOL}
            }),
        )
        .texts(&[
            "Read the notes file.",
            "/tmp/notes.txt",
            "remember the milk",
            "Now read it with the other tool.",
        ])
        .tools(&[DOTTED_TOOL, LONG_TOOL]),
        // A Codex turn: the free-form `apply_patch` tool next to a function,
        // one finished call of each in the history, the custom tool forced,
        // and the platform fields Codex sends with every request.
        ClientRequest::new(
            "custom_tool",
            json!({
                "model": MODEL,
                "instructions": "You are Codex.",
                "input": [
                    user("Delete the stale file."),
                    {"type": "function_call", "call_id": "call_shell000000000000000001",
                     "name": "shell", "arguments": "{\"command\":[\"ls\"]}"},
                    {"type": "function_call_output", "call_id": "call_shell000000000000000001", "output": "stale.txt and old.txt"},
                    {"type": "custom_tool_call", "call_id": "call_patch000000000000000001",
                     "name": "apply_patch", "input": PATCH},
                    {"type": "custom_tool_call_output", "call_id": "call_patch000000000000000001", "output": "Done!"},
                    user("Now delete the other one too.")
                ],
                "tools": [
                    {"type": "function", "name": "shell", "description": "Runs a shell command.", "strict": false,
                     "parameters": {"type": "object", "properties": {
                         "command": {"type": "array", "items": {"type": "string"}}}, "required": ["command"]}},
                    {"type": "custom", "name": "apply_patch", "description": "Edit files with a patch.",
                     "format": {"type": "grammar", "syntax": "lark", "definition": "start: /(.|\\n)+/"}}
                ],
                "tool_choice": {"type": "custom", "name": "apply_patch"},
                "store": false,
                "prompt_cache_key": "0199a1b2-codex-session",
                "service_tier": "priority",
                "stream": true
            }),
        )
        .system(&["You are Codex."])
        .texts(&[
            "Delete the stale file.",
            "stale.txt and old.txt",
            PATCH,
            "Done!",
            "Now delete the other one too.",
        ])
        .tools(&["shell", "apply_patch"])
        .streaming(),
        // `allowed_tools`: three tools declared, the model restricted to two
        // of them. `tools` names the ones that may still be offered.
        ClientRequest::new(
            "allowed_tools",
            json!({
                "model": MODEL,
                "input": "Weather in Oslo, or look it up in the docs.",
                "tools": [weather_tool(), search_tool(),
                    {"type": "function", "name": "delete_account", "description": "Deletes the account.",
                     "parameters": {"type": "object", "properties": {}}, "strict": false}],
                "tool_choice": {"type": "allowed_tools", "mode": "required", "tools": [
                    {"type": "function", "name": "get_weather"},
                    {"type": "function", "name": "search_docs"}
                ]}
            }),
        )
        .texts(&["Weather in Oslo, or look it up in the docs."])
        .tools(&["get_weather", "search_docs"]),
        // An assistant prefill: the input ends with the beginning of the
        // answer the client wants continued.
        ClientRequest::new(
            "prefill",
            json!({
                "model": MODEL,
                "input": [
                    user("Name three primary colours as a list."),
                    assistant("Here they are: 1.")
                ]
            }),
        )
        .texts(&["Name three primary colours as a list."]),
    ]
}

/// The input of the custom tool call in the `custom_tool` scenario (free
/// text, not JSON; on one line so it reads the same inside a JSON string).
pub const PATCH: &str = "*** Begin Patch *** Delete File: stale.txt *** End Patch";

// ---------------------------------------------------------------------------
// Upstream responses
// ---------------------------------------------------------------------------

/// `encrypted_content` of the reasoning items in the canned responses.
pub const ENCRYPTED_REASONING: &str = "gAAAAABoRespBlob3Jlc3BvbnNlcy1lbmNyeXB0ZWQtcmVhc29uaW5n";

fn response_object(id: &str, status: &str, output: Value, usage: Value, details: Value) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": 1759400100,
        "status": status,
        "background": false,
        "error": null,
        "incomplete_details": details,
        "instructions": null,
        "max_output_tokens": null,
        "model": "gpt-5.5-2026-04-14",
        "output": output,
        "parallel_tool_calls": true,
        "previous_response_id": null,
        "reasoning": {"effort": "medium", "summary": null},
        "service_tier": "default",
        "store": false,
        "temperature": 1.0,
        "text": {"format": {"type": "text"}},
        "tool_choice": "auto",
        "tools": [],
        "top_p": 1.0,
        "truncation": "disabled",
        "usage": usage,
        "user": null,
        "metadata": {}
    })
}

fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Value {
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": input + output
    })
}

fn message_item(id: &str, text: &str) -> Value {
    json!({"id": id, "type": "message", "status": "completed", "role": "assistant",
           "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": text}]})
}

const TEXT_SSE: &str = r#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_text01","object":"response","created_at":1759400100,"status":"in_progress","background":false,"error":null,"incomplete_details":null,"model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.in_progress
data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_text01","object":"response","created_at":1759400100,"status":"in_progress","background":false,"error":null,"incomplete_details":null,"model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"msg_text01","type":"message","status":"in_progress","content":[],"role":"assistant"}}

event: response.content_part.added
data: {"type":"response.content_part.added","sequence_number":3,"item_id":"msg_text01","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_text01","output_index":0,"content_index":0,"delta":"The capital of France","logprobs":[],"obfuscation":"aB3dE5"}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_text01","output_index":0,"content_index":0,"delta":" is Paris.","logprobs":[],"obfuscation":"xY9"}

event: response.output_text.done
data: {"type":"response.output_text.done","sequence_number":6,"item_id":"msg_text01","output_index":0,"content_index":0,"text":"The capital of France is Paris.","logprobs":[]}

event: response.content_part.done
data: {"type":"response.content_part.done","sequence_number":7,"item_id":"msg_text01","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"The capital of France is Paris."}}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_text01","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"The capital of France is Paris."}],"role":"assistant"}}

event: response.completed
data: {"type":"response.completed","sequence_number":9,"response":{"id":"resp_text01","object":"response","created_at":1759400100,"status":"completed","background":false,"error":null,"incomplete_details":null,"model":"gpt-5.5-2026-04-14","output":[{"id":"msg_text01","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"The capital of France is Paris."}],"role":"assistant"}],"usage":{"input_tokens":14,"input_tokens_details":{"cached_tokens":0},"output_tokens":7,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":21}}}

"#;

const USAGE_SSE: &str = r#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_usage01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.in_progress
data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_usage01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"msg_usage01","type":"message","status":"in_progress","content":[],"role":"assistant"}}

event: response.content_part.added
data: {"type":"response.content_part.added","sequence_number":3,"item_id":"msg_usage01","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_usage01","output_index":0,"content_index":0,"delta":"Cached answer.","logprobs":[]}

event: response.output_text.done
data: {"type":"response.output_text.done","sequence_number":5,"item_id":"msg_usage01","output_index":0,"content_index":0,"text":"Cached answer.","logprobs":[]}

event: response.content_part.done
data: {"type":"response.content_part.done","sequence_number":6,"item_id":"msg_usage01","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"Cached answer."}}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":7,"output_index":0,"item":{"id":"msg_usage01","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Cached answer."}],"role":"assistant"}}

event: response.completed
data: {"type":"response.completed","sequence_number":8,"response":{"id":"resp_usage01","object":"response","created_at":1759400100,"status":"completed","model":"gpt-5.5-2026-04-14","output":[{"id":"msg_usage01","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Cached answer."}],"role":"assistant"}],"usage":{"input_tokens":1000,"input_tokens_details":{"cached_tokens":800,"cache_write_tokens":150},"output_tokens":120,"output_tokens_details":{"reasoning_tokens":64},"total_tokens":1120}}}

"#;

const REASONING_SSE: &str = r#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_reason01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.in_progress
data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_reason01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"rs_reason01","type":"reasoning","encrypted_content":"gAAAAABoPartial","summary":[]}}

event: response.reasoning_summary_part.added
data: {"type":"response.reasoning_summary_part.added","sequence_number":3,"item_id":"rs_reason01","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}

event: response.reasoning_summary_text.delta
data: {"type":"response.reasoning_summary_text.delta","sequence_number":4,"item_id":"rs_reason01","output_index":0,"summary_index":0,"delta":"Assume it is rational, "}

event: response.reasoning_summary_text.delta
data: {"type":"response.reasoning_summary_text.delta","sequence_number":5,"item_id":"rs_reason01","output_index":0,"summary_index":0,"delta":"then derive a contradiction."}

event: response.reasoning_summary_text.done
data: {"type":"response.reasoning_summary_text.done","sequence_number":6,"item_id":"rs_reason01","output_index":0,"summary_index":0,"text":"Assume it is rational, then derive a contradiction."}

event: response.reasoning_summary_part.done
data: {"type":"response.reasoning_summary_part.done","sequence_number":7,"item_id":"rs_reason01","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":"Assume it is rational, then derive a contradiction."}}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"rs_reason01","type":"reasoning","encrypted_content":"gAAAAABoRespBlob3Jlc3BvbnNlcy1lbmNyeXB0ZWQtcmVhc29uaW5n","summary":[{"type":"summary_text","text":"Assume it is rational, then derive a contradiction."}]}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"msg_reason01","type":"message","status":"in_progress","content":[],"role":"assistant"}}

event: response.content_part.added
data: {"type":"response.content_part.added","sequence_number":10,"item_id":"msg_reason01","output_index":1,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":11,"item_id":"msg_reason01","output_index":1,"content_index":0,"delta":"It is irrational: ","logprobs":[]}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":12,"item_id":"msg_reason01","output_index":1,"content_index":0,"delta":"no fraction squares to 2.","logprobs":[]}

event: response.output_text.done
data: {"type":"response.output_text.done","sequence_number":13,"item_id":"msg_reason01","output_index":1,"content_index":0,"text":"It is irrational: no fraction squares to 2.","logprobs":[]}

event: response.content_part.done
data: {"type":"response.content_part.done","sequence_number":14,"item_id":"msg_reason01","output_index":1,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"It is irrational: no fraction squares to 2."}}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":15,"output_index":1,"item":{"id":"msg_reason01","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"It is irrational: no fraction squares to 2."}],"role":"assistant"}}

event: response.completed
data: {"type":"response.completed","sequence_number":16,"response":{"id":"resp_reason01","object":"response","created_at":1759400100,"status":"completed","model":"gpt-5.5-2026-04-14","output":[{"id":"rs_reason01","type":"reasoning","encrypted_content":"gAAAAABoRespBlob3Jlc3BvbnNlcy1lbmNyeXB0ZWQtcmVhc29uaW5n","summary":[{"type":"summary_text","text":"Assume it is rational, then derive a contradiction."}]},{"id":"msg_reason01","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"It is irrational: no fraction squares to 2."}],"role":"assistant"}],"usage":{"input_tokens":20,"input_tokens_details":{"cached_tokens":0},"output_tokens":50,"output_tokens_details":{"reasoning_tokens":30},"total_tokens":70}}}

"#;

const TOOL_SSE: &str = r#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_tool01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.in_progress
data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_tool01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"fc_tool01","type":"function_call","status":"in_progress","arguments":"","call_id":"call_T1aB2cD3eF4gH5iJ6kL7mN8o","name":"get_weather"}}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","sequence_number":3,"item_id":"fc_tool01","output_index":0,"delta":"{\"location\":"}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_tool01","output_index":0,"delta":"\"Paris\"}"}

event: response.function_call_arguments.done
data: {"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"fc_tool01","output_index":0,"arguments":"{\"location\":\"Paris\"}"}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"fc_tool01","type":"function_call","status":"completed","arguments":"{\"location\":\"Paris\"}","call_id":"call_T1aB2cD3eF4gH5iJ6kL7mN8o","name":"get_weather"}}

event: response.completed
data: {"type":"response.completed","sequence_number":7,"response":{"id":"resp_tool01","object":"response","created_at":1759400100,"status":"completed","model":"gpt-5.5-2026-04-14","output":[{"id":"fc_tool01","type":"function_call","status":"completed","arguments":"{\"location\":\"Paris\"}","call_id":"call_T1aB2cD3eF4gH5iJ6kL7mN8o","name":"get_weather"}],"usage":{"input_tokens":60,"input_tokens_details":{"cached_tokens":0},"output_tokens":18,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":78}}}

"#;

const PARALLEL_SSE: &str = r#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_par01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.in_progress
data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_par01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"fc_par01","type":"function_call","status":"in_progress","arguments":"","call_id":"call_P1aaaaaaaaaaaaaaaaaaaaaa","name":"get_weather"}}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","sequence_number":3,"item_id":"fc_par01","output_index":0,"delta":"{\"loca"}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_par01","output_index":0,"delta":"tion\":\"Tokyo\"}"}

event: response.function_call_arguments.done
data: {"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"fc_par01","output_index":0,"arguments":"{\"location\":\"Tokyo\"}"}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"fc_par01","type":"function_call","status":"completed","arguments":"{\"location\":\"Tokyo\"}","call_id":"call_P1aaaaaaaaaaaaaaaaaaaaaa","name":"get_weather"}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":7,"output_index":1,"item":{"id":"fc_par02","type":"function_call","status":"in_progress","arguments":"","call_id":"call_P2bbbbbbbbbbbbbbbbbbbbbb","name":"search_docs"}}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","sequence_number":8,"item_id":"fc_par02","output_index":1,"delta":"{\"qu"}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","sequence_number":9,"item_id":"fc_par02","output_index":1,"delta":"ery\":\"tokyo climate\",\"limit\":3}"}

event: response.function_call_arguments.done
data: {"type":"response.function_call_arguments.done","sequence_number":10,"item_id":"fc_par02","output_index":1,"arguments":"{\"query\":\"tokyo climate\",\"limit\":3}"}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":11,"output_index":1,"item":{"id":"fc_par02","type":"function_call","status":"completed","arguments":"{\"query\":\"tokyo climate\",\"limit\":3}","call_id":"call_P2bbbbbbbbbbbbbbbbbbbbbb","name":"search_docs"}}

event: response.completed
data: {"type":"response.completed","sequence_number":12,"response":{"id":"resp_par01","object":"response","created_at":1759400100,"status":"completed","model":"gpt-5.5-2026-04-14","output":[{"id":"fc_par01","type":"function_call","status":"completed","arguments":"{\"location\":\"Tokyo\"}","call_id":"call_P1aaaaaaaaaaaaaaaaaaaaaa","name":"get_weather"},{"id":"fc_par02","type":"function_call","status":"completed","arguments":"{\"query\":\"tokyo climate\",\"limit\":3}","call_id":"call_P2bbbbbbbbbbbbbbbbbbbbbb","name":"search_docs"}],"usage":{"input_tokens":70,"input_tokens_details":{"cached_tokens":0},"output_tokens":40,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":110}}}

"#;

const REASONING_TOOL_SSE: &str = r#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_rt01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.in_progress
data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_rt01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"rs_rt01","type":"reasoning","summary":[]}}

event: response.reasoning_summary_part.added
data: {"type":"response.reasoning_summary_part.added","sequence_number":3,"item_id":"rs_rt01","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}

event: response.reasoning_summary_text.delta
data: {"type":"response.reasoning_summary_text.delta","sequence_number":4,"item_id":"rs_rt01","output_index":0,"summary_index":0,"delta":"I need the weather for Paris."}

event: response.reasoning_summary_text.done
data: {"type":"response.reasoning_summary_text.done","sequence_number":5,"item_id":"rs_rt01","output_index":0,"summary_index":0,"text":"I need the weather for Paris."}

event: response.reasoning_summary_part.done
data: {"type":"response.reasoning_summary_part.done","sequence_number":6,"item_id":"rs_rt01","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":"I need the weather for Paris."}}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":7,"output_index":0,"item":{"id":"rs_rt01","type":"reasoning","encrypted_content":"gAAAAABoRespBlob3Jlc3BvbnNlcy1lbmNyeXB0ZWQtcmVhc29uaW5n","summary":[{"type":"summary_text","text":"I need the weather for Paris."}]}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":8,"output_index":1,"item":{"id":"fc_rt01","type":"function_call","status":"in_progress","arguments":"","call_id":"call_RT1cccccccccccccccccccc","name":"get_weather"}}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","sequence_number":9,"item_id":"fc_rt01","output_index":1,"delta":"{\"location\":\"Paris\"}"}

event: response.function_call_arguments.done
data: {"type":"response.function_call_arguments.done","sequence_number":10,"item_id":"fc_rt01","output_index":1,"arguments":"{\"location\":\"Paris\"}"}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":11,"output_index":1,"item":{"id":"fc_rt01","type":"function_call","status":"completed","arguments":"{\"location\":\"Paris\"}","call_id":"call_RT1cccccccccccccccccccc","name":"get_weather"}}

event: response.completed
data: {"type":"response.completed","sequence_number":12,"response":{"id":"resp_rt01","object":"response","created_at":1759400100,"status":"completed","model":"gpt-5.5-2026-04-14","output":[{"id":"rs_rt01","type":"reasoning","encrypted_content":"gAAAAABoRespBlob3Jlc3BvbnNlcy1lbmNyeXB0ZWQtcmVhc29uaW5n","summary":[{"type":"summary_text","text":"I need the weather for Paris."}]},{"id":"fc_rt01","type":"function_call","status":"completed","arguments":"{\"location\":\"Paris\"}","call_id":"call_RT1cccccccccccccccccccc","name":"get_weather"}],"usage":{"input_tokens":80,"input_tokens_details":{"cached_tokens":0},"output_tokens":45,"output_tokens_details":{"reasoning_tokens":20},"total_tokens":125}}}

"#;

const LENGTH_SSE: &str = r#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_len01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.in_progress
data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_len01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"msg_len01","type":"message","status":"in_progress","content":[],"role":"assistant"}}

event: response.content_part.added
data: {"type":"response.content_part.added","sequence_number":3,"item_id":"msg_len01","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_len01","output_index":0,"content_index":0,"delta":"One, two, three, four,","logprobs":[]}

event: response.output_text.done
data: {"type":"response.output_text.done","sequence_number":5,"item_id":"msg_len01","output_index":0,"content_index":0,"text":"One, two, three, four,","logprobs":[]}

event: response.content_part.done
data: {"type":"response.content_part.done","sequence_number":6,"item_id":"msg_len01","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"One, two, three, four,"}}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":7,"output_index":0,"item":{"id":"msg_len01","type":"message","status":"incomplete","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"One, two, three, four,"}],"role":"assistant"}}

event: response.incomplete
data: {"type":"response.incomplete","sequence_number":8,"response":{"id":"resp_len01","object":"response","created_at":1759400100,"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"model":"gpt-5.5-2026-04-14","output":[{"id":"msg_len01","type":"message","status":"incomplete","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"One, two, three, four,"}],"role":"assistant"}],"usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":0},"output_tokens":8,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":20}}}

"#;

const FILTER_SSE: &str = r#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_cf01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.in_progress
data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_cf01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"msg_cf01","type":"message","status":"in_progress","content":[],"role":"assistant"}}

event: response.content_part.added
data: {"type":"response.content_part.added","sequence_number":3,"item_id":"msg_cf01","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_cf01","output_index":0,"content_index":0,"delta":"Here is how","logprobs":[]}

event: response.output_text.done
data: {"type":"response.output_text.done","sequence_number":5,"item_id":"msg_cf01","output_index":0,"content_index":0,"text":"Here is how","logprobs":[]}

event: response.content_part.done
data: {"type":"response.content_part.done","sequence_number":6,"item_id":"msg_cf01","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"Here is how"}}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":7,"output_index":0,"item":{"id":"msg_cf01","type":"message","status":"incomplete","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Here is how"}],"role":"assistant"}}

event: response.incomplete
data: {"type":"response.incomplete","sequence_number":8,"response":{"id":"resp_cf01","object":"response","created_at":1759400100,"status":"incomplete","incomplete_details":{"reason":"content_filter"},"model":"gpt-5.5-2026-04-14","output":[{"id":"msg_cf01","type":"message","status":"incomplete","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Here is how"}],"role":"assistant"}],"usage":{"input_tokens":15,"input_tokens_details":{"cached_tokens":0},"output_tokens":3,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":18}}}

"#;

const REFUSAL_SSE: &str = r#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_ref01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.in_progress
data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_ref01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"msg_ref01","type":"message","status":"in_progress","content":[],"role":"assistant"}}

event: response.content_part.added
data: {"type":"response.content_part.added","sequence_number":3,"item_id":"msg_ref01","output_index":0,"content_index":0,"part":{"type":"refusal","refusal":""}}

event: response.refusal.delta
data: {"type":"response.refusal.delta","sequence_number":4,"item_id":"msg_ref01","output_index":0,"content_index":0,"delta":"I cannot help"}

event: response.refusal.delta
data: {"type":"response.refusal.delta","sequence_number":5,"item_id":"msg_ref01","output_index":0,"content_index":0,"delta":" with that request."}

event: response.refusal.done
data: {"type":"response.refusal.done","sequence_number":6,"item_id":"msg_ref01","output_index":0,"content_index":0,"refusal":"I cannot help with that request."}

event: response.content_part.done
data: {"type":"response.content_part.done","sequence_number":7,"item_id":"msg_ref01","output_index":0,"content_index":0,"part":{"type":"refusal","refusal":"I cannot help with that request."}}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_ref01","type":"message","status":"completed","content":[{"type":"refusal","refusal":"I cannot help with that request."}],"role":"assistant"}}

event: response.completed
data: {"type":"response.completed","sequence_number":9,"response":{"id":"resp_ref01","object":"response","created_at":1759400100,"status":"completed","model":"gpt-5.5-2026-04-14","output":[{"id":"msg_ref01","type":"message","status":"completed","content":[{"type":"refusal","refusal":"I cannot help with that request."}],"role":"assistant"}],"usage":{"input_tokens":15,"input_tokens_details":{"cached_tokens":0},"output_tokens":9,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":24}}}

"#;

// The flat SSE `error` event the vendor documents (notes 15 §4.3).
const ERROR_SSE: &str = r#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_err01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.in_progress
data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_err01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"msg_err01","type":"message","status":"in_progress","content":[],"role":"assistant"}}

event: response.content_part.added
data: {"type":"response.content_part.added","sequence_number":3,"item_id":"msg_err01","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_err01","output_index":0,"content_index":0,"delta":"Partial answer","logprobs":[]}

event: error
data: {"type":"error","sequence_number":5,"code":"server_error","message":"The server had an error while processing your request.","param":null}

"#;

const TRUNCATED_SSE: &str = r#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_cut01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.in_progress
data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_cut01","object":"response","created_at":1759400100,"status":"in_progress","model":"gpt-5.5-2026-04-14","output":[],"usage":null}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"msg_cut01","type":"message","status":"in_progress","content":[],"role":"assistant"}}

event: response.content_part.added
data: {"type":"response.content_part.added","sequence_number":3,"item_id":"msg_cut01","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_cut01","output_index":0,"content_index":0,"delta":"The answer is fo","logprobs":[]}

"#;

pub fn responses() -> Vec<UpstreamResponse> {
    let weather_call = |id: &str, call_id: &str, location: &str| {
        json!({"id": id, "type": "function_call", "status": "completed",
               "arguments": format!("{{\"location\":\"{location}\"}}"),
               "call_id": call_id, "name": "get_weather"})
    };
    vec![
        UpstreamResponse::new(
            "text",
            response_object(
                "resp_text01",
                "completed",
                json!([message_item(
                    "msg_text01",
                    "The capital of France is Paris."
                )]),
                usage(14, 0, 7, 0),
                Value::Null,
            ),
            TEXT_SSE,
            Expect::text("The capital of France is Paris.").usage(14, 0, 0, 7, 0),
        ),
        UpstreamResponse::new(
            "text_usage",
            response_object(
                "resp_usage01",
                "completed",
                json!([message_item("msg_usage01", "Cached answer.")]),
                json!({
                    "input_tokens": 1000,
                    "input_tokens_details": {"cached_tokens": 800, "cache_write_tokens": 150},
                    "output_tokens": 120,
                    "output_tokens_details": {"reasoning_tokens": 64},
                    "total_tokens": 1120
                }),
                Value::Null,
            ),
            USAGE_SSE,
            Expect::text("Cached answer.").usage(50, 800, 150, 120, 64),
        ),
        UpstreamResponse::new(
            "reasoning_text",
            response_object(
                "resp_reason01",
                "completed",
                json!([
                    {"id": "rs_reason01", "type": "reasoning", "encrypted_content": ENCRYPTED_REASONING,
                     "summary": [{"type": "summary_text", "text": "Assume it is rational, then derive a contradiction."}]},
                    message_item("msg_reason01", "It is irrational: no fraction squares to 2.")
                ]),
                usage(20, 0, 50, 30),
                Value::Null,
            ),
            REASONING_SSE,
            Expect::text("It is irrational: no fraction squares to 2.")
                .reasoning("Assume it is rational, then derive a contradiction.")
                .usage(20, 0, 0, 50, 30)
                .blob(ENCRYPTED_REASONING),
        ),
        UpstreamResponse::new(
            "tool_call",
            response_object(
                "resp_tool01",
                "completed",
                json!([weather_call(
                    "fc_tool01",
                    "call_T1aB2cD3eF4gH5iJ6kL7mN8o",
                    "Paris"
                )]),
                usage(60, 0, 18, 0),
                Value::Null,
            ),
            TOOL_SSE,
            Expect::text("")
                .call("get_weather", json!({"location": "Paris"}))
                .usage(60, 0, 0, 18, 0),
        ),
        UpstreamResponse::new(
            "parallel_tools",
            response_object(
                "resp_par01",
                "completed",
                json!([
                    weather_call("fc_par01", "call_P1aaaaaaaaaaaaaaaaaaaaaa", "Tokyo"),
                    {"id": "fc_par02", "type": "function_call", "status": "completed",
                     "arguments": "{\"query\":\"tokyo climate\",\"limit\":3}",
                     "call_id": "call_P2bbbbbbbbbbbbbbbbbbbbbb", "name": "search_docs"}
                ]),
                usage(70, 0, 40, 0),
                Value::Null,
            ),
            PARALLEL_SSE,
            Expect::text("")
                .call("get_weather", json!({"location": "Tokyo"}))
                .call("search_docs", json!({"query": "tokyo climate", "limit": 3}))
                .usage(70, 0, 0, 40, 0),
        ),
        // Native placement: the reasoning item, with its encrypted content,
        // directly ahead of the call it led to.
        UpstreamResponse::new(
            "reasoning_tool",
            response_object(
                "resp_rt01",
                "completed",
                json!([
                    {"id": "rs_rt01", "type": "reasoning", "encrypted_content": ENCRYPTED_REASONING,
                     "summary": [{"type": "summary_text", "text": "I need the weather for Paris."}]},
                    weather_call("fc_rt01", "call_RT1cccccccccccccccccccc", "Paris")
                ]),
                usage(80, 0, 45, 20),
                Value::Null,
            ),
            REASONING_TOOL_SSE,
            Expect::text("")
                .reasoning("I need the weather for Paris.")
                .call("get_weather", json!({"location": "Paris"}))
                .usage(80, 0, 0, 45, 20)
                .blob(ENCRYPTED_REASONING),
        ),
        UpstreamResponse::new(
            "length",
            response_object(
                "resp_len01",
                "incomplete",
                json!([{"id": "msg_len01", "type": "message", "status": "incomplete", "role": "assistant",
                        "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": "One, two, three, four,"}]}]),
                usage(12, 0, 8, 0),
                json!({"reason": "max_output_tokens"}),
            ),
            LENGTH_SSE,
            Expect::text("One, two, three, four,")
                .finish(FinishReason::Length)
                .usage(12, 0, 0, 8, 0),
        ),
        UpstreamResponse::new(
            "content_filter",
            response_object(
                "resp_cf01",
                "incomplete",
                json!([{"id": "msg_cf01", "type": "message", "status": "incomplete", "role": "assistant",
                        "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": "Here is how"}]}]),
                usage(15, 0, 3, 0),
                json!({"reason": "content_filter"}),
            ),
            FILTER_SSE,
            Expect::text("Here is how")
                .finish(FinishReason::ContentFilter)
                .usage(15, 0, 0, 3, 0),
        ),
        UpstreamResponse::new(
            "refusal",
            response_object(
                "resp_ref01",
                "completed",
                json!([{"id": "msg_ref01", "type": "message", "status": "completed", "role": "assistant",
                        "content": [{"type": "refusal", "refusal": "I cannot help with that request."}]}]),
                usage(15, 0, 9, 0),
                Value::Null,
            ),
            REFUSAL_SSE,
            Expect::text("")
                .refusal("I cannot help with that request.")
                .finish(FinishReason::Refusal)
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
