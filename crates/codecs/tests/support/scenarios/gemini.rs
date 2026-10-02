//! Gemini `generateContent`: requests as the `google-genai` SDKs and Gemini
//! CLI write them, and responses as the API answers.
//!
//! The model and the stream flag are not part of a Gemini body. A scenario
//! that streams says so with a private `__stream` key, which the harness
//! turns into the request path and removes.

use super::{
    ClientRequest, DOTTED_TOOL, Expect, IMAGE_URL, LONG_TOOL, MediaKind, PDF_B64, PNG_B64,
    StreamEnd, TOOL_PNG_B64, UpstreamResponse, WEATHER_QUESTION,
};
use serde_json::{Value, json};
use switchyard_core::ir::FinishReason;
use switchyard_core::reasoning::{Depth, Effort};

fn weather_declaration() -> Value {
    json!({
        "name": "get_weather",
        "description": "Get the current weather for a city.",
        "parameters": {
            "type": "OBJECT",
            "properties": {
                "location": {"type": "STRING", "description": "City name"},
                "unit": {"type": "STRING", "enum": ["celsius", "fahrenheit"], "nullable": true}
            },
            "required": ["location"]
        }
    })
}

fn search_declaration() -> Value {
    json!({
        "name": "search_docs",
        "description": "Search the documentation.",
        "parametersJsonSchema": {
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer"}
            },
            "required": ["query"]
        }
    })
}

fn user(text: &str) -> Value {
    json!({"role": "user", "parts": [{"text": text}]})
}

fn model(text: &str) -> Value {
    json!({"role": "model", "parts": [{"text": text}]})
}

/// The `ClientCtx::request` of the response-side matrix.
pub fn tool_request() -> Value {
    json!({
        "contents": [user(WEATHER_QUESTION)],
        "tools": [{"functionDeclarations": [weather_declaration(), search_declaration()]}]
    })
}

pub fn requests() -> Vec<ClientRequest> {
    vec![
        ClientRequest::new(
            "plain",
            json!({"contents": [{"parts": [{"text": "What is the capital of France?"}]}]}),
        )
        .texts(&["What is the capital of France?"]),
        ClientRequest::new(
            "system",
            json!({
                "systemInstruction": {"parts": [
                    {"text": "You are a terse assistant."},
                    {"text": "Answer in one sentence."}
                ]},
                "contents": [user("Why is the sky blue?")]
            }),
        )
        .system(&["You are a terse assistant.", "Answer in one sentence."])
        .texts(&["Why is the sky blue?"]),
        // The Python SDK writes snake_case.
        ClientRequest::new(
            "multi_turn",
            json!({
                "system_instruction": {"parts": [{"text": "You are friendly."}]},
                "contents": [
                    user("My name is Ada."),
                    model("Nice to meet you, Ada."),
                    user("What is my name?")
                ],
                "generation_config": {"max_output_tokens": 1024},
                "__stream": true
            }),
        )
        .system(&["You are friendly."])
        .texts(&["My name is Ada.", "Nice to meet you, Ada.", "What is my name?"])
        .streaming(),
        ClientRequest::new(
            "image_url",
            json!({
                "contents": [{"role": "user", "parts": [
                    {"text": "What is in this picture?"},
                    {"fileData": {"mimeType": "image/png", "fileUri": IMAGE_URL}}
                ]}]
            }),
        )
        .texts(&["What is in this picture?"])
        .media(MediaKind::ImageUrl, IMAGE_URL),
        ClientRequest::new(
            "image_base64",
            json!({
                "contents": [{"role": "user", "parts": [
                    {"inlineData": {"mimeType": "image/png", "data": PNG_B64}},
                    {"text": "Describe this image."}
                ]}]
            }),
        )
        .texts(&["Describe this image."])
        .media(MediaKind::ImageBase64, PNG_B64),
        ClientRequest::new(
            "pdf",
            json!({
                "contents": [{"role": "user", "parts": [
                    {"inline_data": {"mime_type": "application/pdf", "data": PDF_B64}},
                    {"text": "Summarise the attached document."}
                ]}]
            }),
        )
        .texts(&["Summarise the attached document."])
        .media(MediaKind::Pdf, PDF_B64),
        ClientRequest::new(
            "tools_auto",
            json!({
                "contents": [user(WEATHER_QUESTION)],
                "tools": [{"functionDeclarations": [weather_declaration(), search_declaration()]}],
                "toolConfig": {"functionCallingConfig": {"mode": "AUTO"}}
            }),
        )
        .texts(&[WEATHER_QUESTION])
        .tools(&["get_weather", "search_docs"]),
        ClientRequest::new(
            "tool_forced",
            json!({
                "contents": [user("Look up the weather in Oslo.")],
                "tools": [{"functionDeclarations": [weather_declaration(), search_declaration()]}],
                "toolConfig": {"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["get_weather"]}},
                "generationConfig": {"thinkingConfig": {"thinkingBudget": 2048}}
            }),
        )
        .texts(&["Look up the weather in Oslo."])
        .tools(&["get_weather", "search_docs"])
        .depth(Depth::Budget(2048)),
        ClientRequest::new(
            "tool_required",
            json!({
                "contents": [user("Use a tool to answer: weather in Lima?")],
                "tools": [{"functionDeclarations": [weather_declaration()]}],
                "toolConfig": {"functionCallingConfig": {"mode": "ANY"}}
            }),
        )
        .texts(&["Use a tool to answer: weather in Lima?"])
        .tools(&["get_weather"]),
        // A Gemini 3 tool loop: the first call of the model step carries the
        // thought signature, ids pair calls and responses.
        ClientRequest::new(
            "tool_loop",
            json!({
                "systemInstruction": {"parts": [{"text": "You are a weather bot."}]},
                "contents": [
                    user(WEATHER_QUESTION),
                    {"role": "model", "parts": [
                        {"text": "Two cities, two calls.", "thought": true},
                        {"functionCall": {"name": "get_weather", "args": {"location": "Paris", "unit": "celsius"}, "id": "fc-paris-1"},
                         "thoughtSignature": "CiQBjz1rXGdlbWluaS1yZXF1ZXN0LWxvb3Atc2lnbmF0dXJlLTAwMDI="},
                        {"functionCall": {"name": "get_weather", "args": {"location": "Tokyo"}, "id": "fc-tokyo-2"}}
                    ]},
                    {"role": "user", "parts": [
                        {"functionResponse": {"name": "get_weather", "id": "fc-paris-1",
                                              "response": {"output": "15 degrees and cloudy"}}},
                        {"functionResponse": {"name": "get_weather", "id": "fc-tokyo-2",
                                              "response": {"output": "22 degrees and sunny"}}}
                    ]},
                    model("Paris is cloudy at 15, Tokyo sunny at 22."),
                    user("And what about Berlin?")
                ],
                "tools": [{"functionDeclarations": [weather_declaration()]}]
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
        // Older models: no call ids, pairing by name and position.
        ClientRequest::new(
            "tool_error",
            json!({
                "contents": [
                    user("Search the docs for the retry policy."),
                    {"role": "model", "parts": [
                        {"functionCall": {"name": "search_docs", "args": {"query": "retry policy"}}}
                    ]},
                    {"role": "user", "parts": [
                        {"functionResponse": {"name": "search_docs",
                                              "response": {"error": "Error: index unavailable (ETIMEDOUT)"}}}
                    ]}
                ],
                "tools": [{"functionDeclarations": [search_declaration()]}]
            }),
        )
        .texts(&[
            "Search the docs for the retry policy.",
            "retry policy",
            "Error: index unavailable (ETIMEDOUT)",
        ])
        .tools(&["search_docs"]),
        // The multimodal function response of the newest models.
        ClientRequest::new(
            "tool_image",
            json!({
                "contents": [
                    user("Take a screenshot of the page."),
                    {"role": "model", "parts": [{"functionCall": {"name": "screenshot", "args": {}}}]},
                    {"role": "user", "parts": [
                        {"functionResponse": {"name": "screenshot",
                                              "response": {"output": "Screenshot captured"},
                                              "parts": [{"inlineData": {"mimeType": "image/png", "data": TOOL_PNG_B64}}]}}
                    ]}
                ],
                "tools": [{"functionDeclarations": [{"name": "screenshot", "description": "Capture the page."}]}]
            }),
        )
        .texts(&["Take a screenshot of the page.", "Screenshot captured"])
        .tools(&["screenshot"])
        .media(MediaKind::ToolImage, TOOL_PNG_B64),
        ClientRequest::new(
            "reasoning_budget",
            json!({
                "contents": [user("Prove that the square root of 2 is irrational.")],
                "generationConfig": {
                    "maxOutputTokens": 16000,
                    "thinkingConfig": {"thinkingBudget": 8192, "includeThoughts": true}
                }
            }),
        )
        .texts(&["Prove that the square root of 2 is irrational."])
        .depth(Depth::Budget(8192)),
        ClientRequest::new(
            "reasoning_level",
            json!({
                "contents": [user("Prove that there are infinitely many primes.")],
                "generationConfig": {"thinkingConfig": {"thinkingLevel": "high"}}
            }),
        )
        .texts(&["Prove that there are infinitely many primes."])
        .depth(Depth::Level(Effort::High)),
        ClientRequest::new(
            "reasoning_none",
            json!({
                "contents": [user("Reply with the single word ok.")],
                "generation_config": {"thinking_config": {"thinking_budget": 0}}
            }),
        )
        .texts(&["Reply with the single word ok."])
        .depth(Depth::Off),
        ClientRequest::new(
            "reasoning_dynamic",
            json!({
                "contents": [user("Think as much as you need: is 91 prime?")],
                "generationConfig": {"thinkingConfig": {"thinkingBudget": -1}}
            }),
        )
        .texts(&["Think as much as you need: is 91 prime?"])
        .depth(Depth::Auto),
        ClientRequest::new(
            "json_object",
            json!({
                "systemInstruction": {"parts": [{"text": "Reply in JSON."}]},
                "contents": [user("List three primary colours.")],
                "generationConfig": {"responseMimeType": "application/json"}
            }),
        )
        .system(&["Reply in JSON."])
        .texts(&["List three primary colours."]),
        // `responseSchema` in Gemini's own dialect (upper-case types).
        ClientRequest::new(
            "json_schema",
            json!({
                "contents": [user("Extract: Ada Lovelace, born 1815.")],
                "generationConfig": {
                    "responseMimeType": "application/json",
                    "responseSchema": {
                        "type": "OBJECT",
                        "properties": {
                            "name": {"type": "STRING"},
                            "born": {"type": "INTEGER"}
                        },
                        "required": ["name", "born"],
                        "propertyOrdering": ["name", "born"]
                    }
                }
            }),
        )
        .texts(&["Extract: Ada Lovelace, born 1815."]),
        ClientRequest::new(
            "sampling",
            json!({
                "contents": [user("Write a haiku about rain.")],
                "generationConfig": {
                    "temperature": 1.5,
                    "topP": 0.9,
                    "topK": 40,
                    "maxOutputTokens": 256,
                    "seed": 42,
                    "candidateCount": 1,
                    "presencePenalty": 0.5,
                    "frequencyPenalty": 0.25
                },
                "safetySettings": [{"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_ONLY_HIGH"}]
            }),
        )
        .texts(&["Write a haiku about rain."]),
        ClientRequest::new(
            "stop",
            json!({
                "contents": [user("Count from one to ten.")],
                "generationConfig": {"stopSequences": ["seven", "STOP"], "maxOutputTokens": 100}
            }),
        )
        .texts(&["Count from one to ten."]),
        ClientRequest::new(
            "long_conversation",
            json!({
                "contents": [
                    user("Turn one alpha."),
                    user("Turn two beta."),
                    model("Reply three gamma."),
                    model("Reply four delta."),
                    user("Turn five epsilon."),
                    {"parts": [{"text": "Turn six zeta."}]},
                    user("Turn seven eta."),
                    model("Reply eight theta."),
                    user("Turn nine iota."),
                    model("Reply ten kappa."),
                    model("Reply eleven lambda."),
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
        // Not a Gemini role, but clients ported from other APIs send it and
        // the gateway accepts it.
        ClientRequest::new(
            "mid_system",
            json!({
                "systemInstruction": {"role": "system", "parts": [{"text": "You are a helpful assistant."}]},
                "contents": [
                    user("Tell me a joke."),
                    model("Why did the chicken cross the road?"),
                    {"role": "system", "parts": [{"text": "From now on answer in French."}]},
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
                "systemInstruction": {"parts": [{"text": ""}]},
                "contents": [
                    {"role": "user", "parts": [{"text": ""}]},
                    {"role": "model", "parts": [{"text": ""}]},
                    {"role": "user", "parts": [{"text": "   "}]},
                    {"role": "model", "parts": [
                        {"text": "\n\n"},
                        {"functionCall": {"name": "get_weather", "args": {}}}
                    ]},
                    {"role": "user", "parts": [
                        {"functionResponse": {"name": "get_weather", "response": {}}}
                    ]},
                    {"role": "user", "parts": [{"text": ""}, {"text": "Is anybody there?"}]}
                ],
                "tools": [{"functionDeclarations": [weather_declaration()]}]
            }),
        )
        .texts(&["Is anybody there?"])
        .tools(&["get_weather"]),
        ClientRequest::new(
            "unicode",
            json!({
                "systemInstruction": {"parts": [{"text": "Réponds en français. 请用中文回答。"}]},
                "contents": [user("Привет, 世界! 🌍 — ça va? ¿Qué tal? 😀👍🏽 مرحبا")]
            }),
        )
        .system(&["Réponds en français. 请用中文回答。"])
        .texts(&["Привет, 世界! 🌍 — ça va? ¿Qué tal? 😀👍🏽 مرحبا"]),
        // Dots, colons and dashes are native here; 100 characters are not
        // (the limit is 64), but nothing stops a client from sending them.
        ClientRequest::new(
            "odd_tool_names",
            json!({
                "contents": [
                    user("Read the notes file."),
                    {"role": "model", "parts": [
                        {"functionCall": {"name": DOTTED_TOOL, "args": {"path": "/tmp/notes.txt"}}}
                    ]},
                    {"role": "user", "parts": [
                        {"functionResponse": {"name": DOTTED_TOOL, "response": {"output": "remember the milk"}}}
                    ]},
                    model("It says to remember the milk."),
                    user("Now read it with the other tool.")
                ],
                "tools": [{"functionDeclarations": [
                    {"name": DOTTED_TOOL, "description": "Read a file.",
                     "parameters": {"type": "OBJECT", "properties": {"path": {"type": "STRING"}}, "required": ["path"]}},
                    {"name": LONG_TOOL, "description": "Read a file, verbosely.",
                     "parameters": {"type": "OBJECT", "properties": {"path": {"type": "STRING"}}}}
                ]}],
                "toolConfig": {"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": [LONG_TOOL]}}
            }),
        )
        .texts(&[
            "Read the notes file.",
            "/tmp/notes.txt",
            "remember the milk",
            "Now read it with the other tool.",
        ])
        .tools(&[DOTTED_TOOL, LONG_TOOL]),
        // Three functions declared, the model restricted to two of them.
        // `tools` names the ones that may still be offered.
        ClientRequest::new(
            "allowed_functions",
            json!({
                "contents": [user("Weather in Oslo, or look it up in the docs.")],
                "tools": [{"functionDeclarations": [
                    weather_declaration(), search_declaration(),
                    {"name": "delete_account", "description": "Deletes the account."}
                ]}],
                "toolConfig": {"functionCallingConfig": {
                    "mode": "ANY", "allowedFunctionNames": ["get_weather", "search_docs"]
                }}
            }),
        )
        .texts(&["Weather in Oslo, or look it up in the docs."])
        .tools(&["get_weather", "search_docs"]),
        // Structured output whose root is a list: the example of Google's
        // own guide. `recipeName` must reach the upstream, in a native
        // schema or in an instruction.
        ClientRequest::new(
            "json_schema_list",
            json!({
                "contents": [user("List two popular cookie recipes.")],
                "generationConfig": {
                    "responseMimeType": "application/json",
                    "responseSchema": {"type": "ARRAY", "items": {
                        "type": "OBJECT",
                        "properties": {
                            "recipeName": {"type": "STRING"},
                            "ingredients": {"type": "ARRAY", "items": {"type": "STRING"}}
                        },
                        "required": ["recipeName"]
                    }}
                }
            }),
        )
        .texts(&["List two popular cookie recipes.", "recipeName"]),
        // Enum output: the answer is one bare value of the list.
        ClientRequest::new(
            "enum_output",
            json!({
                "contents": [user("What kind of instrument is an oboe?")],
                "generationConfig": {
                    "responseMimeType": "text/x.enum",
                    "responseSchema": {"type": "STRING", "enum": ["Percussion", "String", "Woodwind"]}
                }
            }),
        )
        .texts(&["What kind of instrument is an oboe?", "Woodwind"]),
    ]
}

// ---------------------------------------------------------------------------
// Upstream responses
// ---------------------------------------------------------------------------

/// `thoughtSignature` of the canned responses (base64, as the field is
/// protobuf `bytes`).
pub const THOUGHT_SIGNATURE: &str = "CiQBjz1rXGdlbWluaS1yZXNwb25zZS10aG91Z2h0LXNpZ25hdHVyZS0wMQ==";

const TEXT_SSE: &str = r#"data: {"candidates":[{"content":{"parts":[{"text":"The capital of France"}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":14,"totalTokenCount":14},"modelVersion":"gemini-2.5-pro","responseId":"gemText01"}

data: {"candidates":[{"content":{"parts":[{"text":" is Paris."}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":14,"candidatesTokenCount":7,"totalTokenCount":21,"promptTokensDetails":[{"modality":"TEXT","tokenCount":14}]},"modelVersion":"gemini-2.5-pro","responseId":"gemText01"}

"#;

// The test vector of notes 09 §2.1: prompt 100 of which 91 cached,
// candidates 7; with 42 thought tokens counted next to the candidates.
const USAGE_SSE: &str = r#"data: {"candidates":[{"content":{"parts":[{"text":"Cached answer."}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":91,"candidatesTokenCount":7,"thoughtsTokenCount":42,"totalTokenCount":149},"modelVersion":"gemini-2.5-pro","responseId":"gemUsage01"}

"#;

// Without function calls the signature arrives on the last part; here on a
// trailing part with empty text (notes 15 §6.5).
const REASONING_SSE: &str = r#"data: {"candidates":[{"content":{"parts":[{"text":"Assume it is rational, ","thought":true}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":20,"totalTokenCount":20},"modelVersion":"gemini-2.5-pro","responseId":"gemReason01"}

data: {"candidates":[{"content":{"parts":[{"text":"then derive a contradiction.","thought":true}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":20,"totalTokenCount":20},"modelVersion":"gemini-2.5-pro","responseId":"gemReason01"}

data: {"candidates":[{"content":{"parts":[{"text":"It is irrational: "}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":20,"totalTokenCount":20},"modelVersion":"gemini-2.5-pro","responseId":"gemReason01"}

data: {"candidates":[{"content":{"parts":[{"text":"no fraction squares to 2."}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":20,"totalTokenCount":20},"modelVersion":"gemini-2.5-pro","responseId":"gemReason01"}

data: {"candidates":[{"content":{"parts":[{"text":"","thoughtSignature":"CiQBjz1rXGdlbWluaS1yZXNwb25zZS10aG91Z2h0LXNpZ25hdHVyZS0wMQ=="}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":20,"thoughtsTokenCount":30,"totalTokenCount":70},"modelVersion":"gemini-2.5-pro","responseId":"gemReason01"}

"#;

const TOOL_SSE: &str = r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_weather","args":{"location":"Paris"},"id":"gemcall-paris-01"}}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":60,"candidatesTokenCount":18,"totalTokenCount":78},"modelVersion":"gemini-2.5-pro","responseId":"gemTool01"}

"#;

// Gemini never fragments arguments: each call arrives whole, here in two
// chunks, the finish reason on a third.
const PARALLEL_SSE: &str = r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_weather","args":{"location":"Tokyo"},"id":"gemcall-tokyo-01"}}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":70,"totalTokenCount":70},"modelVersion":"gemini-2.5-pro","responseId":"gemPar01"}

data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"search_docs","args":{"query":"tokyo climate","limit":3},"id":"gemcall-search-02"}}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":70,"totalTokenCount":70},"modelVersion":"gemini-2.5-pro","responseId":"gemPar01"}

data: {"candidates":[{"content":{"parts":[{"text":""}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":70,"candidatesTokenCount":40,"totalTokenCount":110},"modelVersion":"gemini-2.5-pro","responseId":"gemPar01"}

"#;

const REASONING_TOOL_SSE: &str = r#"data: {"candidates":[{"content":{"parts":[{"text":"I need the weather for Paris.","thought":true}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":80,"totalTokenCount":80},"modelVersion":"gemini-3-pro","responseId":"gemRt01"}

data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_weather","args":{"location":"Paris"},"id":"gemcall-rt-01"},"thoughtSignature":"CiQBjz1rXGdlbWluaS1yZXNwb25zZS10aG91Z2h0LXNpZ25hdHVyZS0wMQ=="}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":80,"candidatesTokenCount":25,"thoughtsTokenCount":20,"totalTokenCount":125},"modelVersion":"gemini-3-pro","responseId":"gemRt01"}

"#;

const LENGTH_SSE: &str = r#"data: {"candidates":[{"content":{"parts":[{"text":"One, two, three, four,"}],"role":"model"},"finishReason":"MAX_TOKENS","index":0}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":8,"totalTokenCount":20},"modelVersion":"gemini-2.5-pro","responseId":"gemLen01"}

"#;

const SAFETY_SSE: &str = r#"data: {"candidates":[{"content":{"parts":[{"text":"Here is how"}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":15,"totalTokenCount":15},"modelVersion":"gemini-2.5-pro","responseId":"gemSafe01"}

data: {"candidates":[{"finishReason":"SAFETY","index":0,"safetyRatings":[{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","probability":"HIGH","blocked":true}]}],"usageMetadata":{"promptTokenCount":15,"candidatesTokenCount":3,"totalTokenCount":18},"modelVersion":"gemini-2.5-pro","responseId":"gemSafe01"}

"#;

// A blocked prompt: HTTP 200, no candidates (notes 15 §6.3).
const BLOCKED_SSE: &str = r#"data: {"promptFeedback":{"blockReason":"PROHIBITED_CONTENT"},"usageMetadata":{"promptTokenCount":9,"totalTokenCount":9},"modelVersion":"gemini-2.5-pro","responseId":"gemBlock01"}

"#;

// An error object in place of a chunk (notes 15 §6.6).
const ERROR_SSE: &str = r#"data: {"candidates":[{"content":{"parts":[{"text":"Partial answer"}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":10,"totalTokenCount":10},"modelVersion":"gemini-2.5-pro","responseId":"gemErr01"}

data: {"error":{"code":503,"message":"The model is overloaded. Please try again later.","status":"UNAVAILABLE"}}

"#;

const TRUNCATED_SSE: &str = r#"data: {"candidates":[{"content":{"parts":[{"text":"The answer is fo"}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":10,"totalTokenCount":10},"modelVersion":"gemini-2.5-pro","responseId":"gemCut01"}

"#;

fn response(id: &str, parts: Value, finish: &str, usage: Value) -> Value {
    json!({
        "candidates": [{
            "content": {"parts": parts, "role": "model"},
            "finishReason": finish,
            "index": 0
        }],
        "usageMetadata": usage,
        "modelVersion": "gemini-2.5-pro",
        "responseId": id
    })
}

pub fn responses() -> Vec<UpstreamResponse> {
    vec![
        UpstreamResponse::new(
            "text",
            response(
                "gemText01",
                json!([{"text": "The capital of France is Paris."}]),
                "STOP",
                json!({"promptTokenCount": 14, "candidatesTokenCount": 7, "totalTokenCount": 21}),
            ),
            TEXT_SSE,
            Expect::text("The capital of France is Paris.").usage(14, 0, 0, 7, 0),
        ),
        UpstreamResponse::new(
            "text_usage",
            response(
                "gemUsage01",
                json!([{"text": "Cached answer."}]),
                "STOP",
                json!({"promptTokenCount": 100, "cachedContentTokenCount": 91, "candidatesTokenCount": 7,
                       "thoughtsTokenCount": 42, "totalTokenCount": 149}),
            ),
            USAGE_SSE,
            // 100 − 91 = 9 uncached; output = candidates 7 + thoughts 42.
            Expect::text("Cached answer.").usage(9, 91, 0, 49, 42),
        ),
        UpstreamResponse::new(
            "reasoning_text",
            response(
                "gemReason01",
                json!([
                    {"text": "Assume it is rational, then derive a contradiction.", "thought": true},
                    {"text": "It is irrational: no fraction squares to 2.", "thoughtSignature": THOUGHT_SIGNATURE}
                ]),
                "STOP",
                json!({"promptTokenCount": 20, "candidatesTokenCount": 20, "thoughtsTokenCount": 30, "totalTokenCount": 70}),
            ),
            REASONING_SSE,
            Expect::text("It is irrational: no fraction squares to 2.")
                .reasoning("Assume it is rational, then derive a contradiction.")
                .usage(20, 0, 0, 50, 30)
                .blob(THOUGHT_SIGNATURE),
        ),
        UpstreamResponse::new(
            "tool_call",
            response(
                "gemTool01",
                json!([{"functionCall": {"name": "get_weather", "args": {"location": "Paris"}, "id": "gemcall-paris-01"}}]),
                "STOP",
                json!({"promptTokenCount": 60, "candidatesTokenCount": 18, "totalTokenCount": 78}),
            ),
            TOOL_SSE,
            Expect::text("")
                .call("get_weather", json!({"location": "Paris"}))
                .usage(60, 0, 0, 18, 0),
        ),
        UpstreamResponse::new(
            "parallel_tools",
            response(
                "gemPar01",
                json!([
                    {"functionCall": {"name": "get_weather", "args": {"location": "Tokyo"}, "id": "gemcall-tokyo-01"}},
                    {"functionCall": {"name": "search_docs", "args": {"query": "tokyo climate", "limit": 3}, "id": "gemcall-search-02"}}
                ]),
                "STOP",
                json!({"promptTokenCount": 70, "candidatesTokenCount": 40, "totalTokenCount": 110}),
            ),
            PARALLEL_SSE,
            Expect::text("")
                .call("get_weather", json!({"location": "Tokyo"}))
                .call("search_docs", json!({"query": "tokyo climate", "limit": 3}))
                .usage(70, 0, 0, 40, 0),
        ),
        // Native placement: the signature sits on the functionCall part
        // itself (notes 15 §6.5).
        UpstreamResponse::new(
            "reasoning_tool",
            json!({
                "candidates": [{
                    "content": {"parts": [
                        {"text": "I need the weather for Paris.", "thought": true},
                        {"functionCall": {"name": "get_weather", "args": {"location": "Paris"}, "id": "gemcall-rt-01"},
                         "thoughtSignature": THOUGHT_SIGNATURE}
                    ], "role": "model"},
                    "finishReason": "STOP",
                    "index": 0
                }],
                "usageMetadata": {"promptTokenCount": 80, "candidatesTokenCount": 25, "thoughtsTokenCount": 20, "totalTokenCount": 125},
                "modelVersion": "gemini-3-pro",
                "responseId": "gemRt01"
            }),
            REASONING_TOOL_SSE,
            Expect::text("")
                .reasoning("I need the weather for Paris.")
                .call("get_weather", json!({"location": "Paris"}))
                .usage(80, 0, 0, 45, 20)
                .blob(THOUGHT_SIGNATURE),
        ),
        UpstreamResponse::new(
            "length",
            response(
                "gemLen01",
                json!([{"text": "One, two, three, four,"}]),
                "MAX_TOKENS",
                json!({"promptTokenCount": 12, "candidatesTokenCount": 8, "totalTokenCount": 20}),
            ),
            LENGTH_SSE,
            Expect::text("One, two, three, four,")
                .finish(FinishReason::Length)
                .usage(12, 0, 0, 8, 0),
        ),
        UpstreamResponse::new(
            "content_filter",
            response(
                "gemSafe01",
                json!([{"text": "Here is how"}]),
                "SAFETY",
                json!({"promptTokenCount": 15, "candidatesTokenCount": 3, "totalTokenCount": 18}),
            ),
            SAFETY_SSE,
            Expect::text("Here is how")
                .finish(FinishReason::ContentFilter)
                .usage(15, 0, 0, 3, 0),
        ),
        UpstreamResponse::new(
            "blocked_prompt",
            json!({
                "promptFeedback": {"blockReason": "PROHIBITED_CONTENT"},
                "usageMetadata": {"promptTokenCount": 9, "totalTokenCount": 9},
                "modelVersion": "gemini-2.5-pro",
                "responseId": "gemBlock01"
            }),
            BLOCKED_SSE,
            Expect::text("")
                .refusal("The prompt was blocked by Gemini (PROHIBITED_CONTENT).")
                .finish(FinishReason::ContentFilter)
                .usage(9, 0, 0, 0, 0),
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
