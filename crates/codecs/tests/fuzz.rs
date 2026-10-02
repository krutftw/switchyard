//! Robustness: deterministic pseudo-random inputs.
//!
//! * Mutated scenario bodies (dropped keys, swapped types, nulls, huge
//!   strings, deep nesting) → `decode_request` of all four codecs returns
//!   `Ok` or a `CodecError`, never panics; whatever decodes also encodes for
//!   every upstream without panicking, and the raw-body helpers survive too.
//! * Mutated upstream responses and stream events → `decode_response` and
//!   the stream decoders never panic, and the stream decoders keep the
//!   sequence contract whatever they are fed.
//! * Random canonical requests → `encode_request` never panics, whatever
//!   the request holds, and for every request a decoder could have produced
//!   from a body its own vendor accepts the output passes the upstream
//!   vendor's validator.
//! * Random canonical responses → `encode_response` and the stream encoders
//!   produce output that passes the client vendor's validators.
//!
//! Everything is seeded: a failure names the seed that reproduces it.

mod support;

use serde_json::{Map, Value, json};
use support::harness::{
    Failures, client_ctx, client_model, decode_stream, encode_stream, fit_reasoning, known_caps,
    no_panic, over_the_wire, parse_sse, request_path, short, upstream_model, validate_response,
    validate_stream, validate_translated_request,
};
use support::scenarios::{self, DOTTED_TOOL, LONG_TOOL, PDF_B64, PNG_B64};
use switchyard_codecs::codec;
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, CustomTool, FinishReason, FunctionTool, MediaPart, MediaSource,
    Message, OpaquePart, Part, Reasoning, RefusalPart, Request, Response, ResponseFormat, Role,
    Signature, TextPart, Tool, ToolCall, ToolCallKind, ToolChoice, ToolResult,
};
use switchyard_core::reasoning::{Depth, Effort, Fitted, ReasoningConfig, Summary};
use switchyard_core::stream::{StreamEvent, response_to_events};
use switchyard_core::{Protocol, SseEvent, UpstreamCtx, Usage};

/// SplitMix64: small, fast, and the same on every platform.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    /// True `percent` times out of a hundred.
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

// ---------------------------------------------------------------------------
// Mutation of JSON bodies
// ---------------------------------------------------------------------------

/// JSON pointers of every node of `value`.
fn pointers(value: &Value, prefix: &str, out: &mut Vec<String>) {
    out.push(prefix.to_string());
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                pointers(child, &format!("{prefix}/{escaped}"), out);
            }
        }
        Value::Array(list) => {
            for (index, child) in list.iter().enumerate() {
                pointers(child, &format!("{prefix}/{index}"), out);
            }
        }
        _ => {}
    }
}

fn nested(depth: usize, object: bool) -> Value {
    let mut value = json!("leaf");
    for _ in 0..depth {
        value = if object {
            json!({"properties": {"x": value}, "items": {}})
        } else {
            json!([value])
        };
    }
    value
}

fn replacement(rng: &mut Rng) -> Value {
    match rng.below(16) {
        0 => Value::Null,
        1 => json!(true),
        2 => json!(false),
        3 => json!(0),
        4 => json!(-1),
        5 => json!(1.5),
        6 => json!(u64::MAX),
        7 => json!(-9_007_199_254_740_993i64),
        8 => json!(""),
        9 => json!("x".repeat(20_000)),
        10 => json!("\u{0}\u{7f}\u{feff}\"\\ \u{1F600}\u{202E}"),
        11 => json!([]),
        12 => json!({}),
        // serde_json refuses to parse documents nested deeper than 128
        // levels, so this is as deep as a body can get.
        13 => nested(100, false),
        14 => nested(60, true),
        _ => json!([null, 1, "two", {"type": "text", "text": "three"}, [[]]]),
    }
}

/// Applies one random mutation somewhere in `body`.
fn mutate(body: &mut Value, rng: &mut Rng) {
    let mut all = Vec::new();
    pointers(body, "", &mut all);
    let pointer = rng.pick(&all).clone();
    let Some(node) = body.pointer_mut(&pointer) else {
        return;
    };
    match rng.below(7) {
        // Replace the node with something of another type.
        0..=2 => *node = replacement(rng),
        // Drop a key / an element.
        3 => match node {
            Value::Object(map) if !map.is_empty() => {
                let key = map
                    .keys()
                    .nth(rng.below(map.len()))
                    .cloned()
                    .unwrap_or_default();
                map.shift_remove(&key);
            }
            Value::Array(list) if !list.is_empty() => {
                list.remove(rng.below(list.len()));
            }
            other => *other = Value::Null,
        },
        // Duplicate or shuffle elements.
        4 => {
            if let Value::Array(list) = node
                && !list.is_empty()
            {
                let index = rng.below(list.len());
                let copy = list[index].clone();
                list.push(copy);
                let other = rng.below(list.len());
                list.swap(index, other);
            } else {
                *node = json!([node.clone(), node.clone()]);
            }
        }
        // Add a stray key.
        5 => {
            if let Value::Object(map) = node {
                let key = *rng.pick(&[
                    "type",
                    "role",
                    "content",
                    "id",
                    "name",
                    "text",
                    "index",
                    "parts",
                    "",
                    "__proto__",
                ]);
                map.insert(key.to_string(), replacement(rng));
            } else {
                *node = json!({"type": node.clone()});
            }
        }
        // Wrap in the other container kind.
        _ => {
            let inner = node.take();
            *node = if rng.chance(50) {
                json!([inner])
            } else {
                json!({"text": inner, "type": "text"})
            };
        }
    }
}

fn mutated(body: &Value, rng: &mut Rng) -> Value {
    let mut body = body.clone();
    for _ in 0..1 + rng.below(4) {
        mutate(&mut body, rng);
    }
    body
}

/// Every raw-body helper and the request decoder of every codec, on one
/// (probably broken) body. Returns what decoded.
fn hammer_request(body: &Value, seed: u64, failures: &mut Failures) {
    let unknown = UpstreamCtx::default();
    for protocol in Protocol::ALL {
        let context = format!("seed {seed} / {} decoder", short(protocol));
        let path = request_path(protocol, false);
        let outcome = no_panic(&context, || {
            let codec = codec(protocol);
            let _ = codec.request_meta(body, &path);
            let _ = codec.read_reasoning(body);
            for fitted in [
                Fitted::Strip,
                Fitted::Use(Depth::Off),
                Fitted::Use(Depth::Auto),
                Fitted::Use(Depth::Level(Effort::High)),
                Fitted::Use(Depth::Budget(5000)),
            ] {
                let mut patched = body.clone();
                codec.write_reasoning(&mut patched, fitted, &unknown);
            }
            let mut patched = body.clone();
            codec.set_request_model(&mut patched, "some-model");
            codec.prepare_passthrough(&mut patched, true, &unknown);
            let mut payload = body.clone();
            codec.rewrite_response_model(&mut payload, "alias");
            codec.decode_request(body, &path)
        });
        let request = match outcome {
            Err(panic) => {
                failures.push(&context, panic);
                continue;
            }
            Ok(Err(_)) => continue,
            Ok(Ok(request)) => request,
        };
        // Whatever a decoder accepts, every encoder must take.
        for upstream in Protocol::ALL {
            let context = format!("seed {seed} / {} -> {}", short(protocol), short(upstream));
            let caps = known_caps(upstream);
            for ctx in [unknown, caps.ctx()] {
                if let Err(panic) = no_panic(&context, || {
                    let _ = codec(upstream).encode_request(&request, &ctx);
                    let _ = codec(upstream).encode_count_request(&request, &ctx);
                }) {
                    failures.push(&context, panic);
                }
            }
        }
    }
}

#[test]
fn mutated_requests_never_panic() {
    let mut failures = Failures::default();
    let mut seed = 0u64;
    for protocol in Protocol::ALL {
        for scenario in scenarios::requests(protocol) {
            for _ in 0..16 {
                seed += 1;
                let mut rng = Rng::new(seed);
                let body = mutated(&scenario.body, &mut rng);
                hammer_request(&body, seed, &mut failures);
            }
        }
    }
    // Bodies that are not objects at all.
    for body in [
        json!(null),
        json!([]),
        json!("text"),
        json!(42),
        json!({}),
        json!([{"role": "user"}]),
    ] {
        seed += 1;
        hammer_request(&body, seed, &mut failures);
    }
    failures.finish("mutated requests");
}

#[test]
fn mutated_responses_never_panic() {
    let mut failures = Failures::default();
    let mut seed = 10_000u64;
    for protocol in Protocol::ALL {
        for fixture in scenarios::responses(protocol) {
            let Some(body) = &fixture.json else {
                continue;
            };
            for _ in 0..24 {
                seed += 1;
                let mut rng = Rng::new(seed);
                let body = mutated(body, &mut rng);
                // Every decoder gets every vendor's (broken) body.
                for decoder in Protocol::ALL {
                    let context = format!("seed {seed} / {} decode_response", short(decoder));
                    let outcome = no_panic(&context, || codec(decoder).decode_response(&body));
                    let response = match outcome {
                        Err(panic) => {
                            failures.push(&context, panic);
                            continue;
                        }
                        Ok(Err(_)) => continue,
                        Ok(Ok(response)) => response,
                    };
                    // And whatever decodes can be shown to every client.
                    for client in Protocol::ALL {
                        let context = format!(
                            "seed {seed} / {} -> {} client",
                            short(decoder),
                            short(client)
                        );
                        let ctx = client_ctx(client, &scenarios::tool_request(client));
                        if let Err(panic) = no_panic(&context, || {
                            let _ = codec(client).encode_response(&response, &ctx);
                            let events = response_to_events(&response);
                            let _ = encode_stream(client, &ctx, &events);
                        }) {
                            failures.push(&context, panic);
                        }
                    }
                }
                let bytes = body.to_string();
                for decoder in Protocol::ALL {
                    let context = format!("seed {seed} / {} decode_error", short(decoder));
                    if let Err(panic) = no_panic(&context, || {
                        let _ = codec(decoder).decode_error(500, bytes.as_bytes());
                        let _ = codec(decoder).decode_count_response(&body);
                    }) {
                        failures.push(&context, panic);
                    }
                }
            }
        }
    }
    failures.finish("mutated responses");
}

/// Mutates a transcript: the JSON of random events is mutated, events are
/// dropped, duplicated, reordered, renamed or replaced by garbage.
fn mutated_events(transcript: &str, rng: &mut Rng) -> Vec<SseEvent> {
    let mut events = parse_sse(transcript);
    for _ in 0..1 + rng.below(4) {
        if events.is_empty() {
            break;
        }
        let index = rng.below(events.len());
        match rng.below(8) {
            0..=2 => {
                if let Ok(mut value) = serde_json::from_str::<Value>(&events[index].data) {
                    mutate(&mut value, rng);
                    events[index].data = value.to_string();
                }
            }
            3 => {
                events.remove(index);
            }
            4 => {
                let copy = events[index].clone();
                events.insert(rng.below(events.len() + 1), copy);
            }
            5 => {
                let other = rng.below(events.len());
                events.swap(index, other);
            }
            6 => {
                events[index].event = Some(
                    (*rng.pick(&[
                        "error",
                        "ping",
                        "message_stop",
                        "response.completed",
                        "content_block_stop",
                        "",
                        "nonsense",
                    ]))
                    .to_string(),
                )
            }
            _ => {
                events[index].data = (*rng.pick(&[
                    "",
                    "[DONE]",
                    "{",
                    "null",
                    "[]",
                    "42",
                    "\"text\"",
                    "{\"error\":null}",
                    "{\"type\":null}",
                ]))
                .to_string()
            }
        }
    }
    events
}

#[test]
fn mutated_streams_keep_the_sequence_contract() {
    let mut failures = Failures::default();
    let mut seed = 20_000u64;
    for protocol in Protocol::ALL {
        for fixture in scenarios::responses(protocol) {
            let Some(transcript) = fixture.sse else {
                continue;
            };
            for _ in 0..16 {
                seed += 1;
                let mut rng = Rng::new(seed);
                let events = mutated_events(transcript, &mut rng);
                for decoder in Protocol::ALL {
                    let context = format!(
                        "seed {seed} / {} stream decoder on a {} stream",
                        short(decoder),
                        short(protocol)
                    );
                    // `decode_stream` panics when the decoder errors or when
                    // the sequence contract is broken; a decoder may only
                    // *return* an error for an unusable stream.
                    let canonical = no_panic(&context, || {
                        let mut stream = codec(decoder).stream_decoder();
                        let mut out: Vec<StreamEvent> = Vec::new();
                        let mut unusable = false;
                        for event in &events {
                            match stream.decode(event) {
                                Ok(decoded) => out.extend(decoded),
                                Err(_) => {
                                    unusable = true;
                                    break;
                                }
                            }
                        }
                        out.extend(stream.finish());
                        (out, unusable)
                    });
                    let (canonical, unusable) = match canonical {
                        Ok(result) => result,
                        Err(panic) => {
                            failures.push(&context, panic);
                            continue;
                        }
                    };
                    if !unusable
                        && let Err(violation) =
                            switchyard_core::stream::validate_sequence(&canonical)
                    {
                        failures.push(&context, format!("sequence contract broken: {violation}"));
                        continue;
                    }
                    // Any sequence a decoder produces can be shown to any client.
                    for client in Protocol::ALL {
                        let context = format!("seed {seed} / {} stream encoder", short(client));
                        let ctx = client_ctx(client, &scenarios::tool_request(client));
                        if let Err(panic) =
                            no_panic(&context, || encode_stream(client, &ctx, &canonical))
                        {
                            failures.push(&context, panic);
                        }
                    }
                }
            }
        }
    }
    failures.finish("mutated streams");
}

// ---------------------------------------------------------------------------
// Random canonical requests
// ---------------------------------------------------------------------------

/// How far a generated request may stray from what a decoder produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// What `decode_request` yields for a body the client's own vendor
    /// accepts: declared tools have distinct names, every call has an id of
    /// its own and is answered by the next user turn, media is what its type
    /// says, limits are positive. The names, ids, schemas and blobs are as
    /// odd as the four vendors allow between them, which is what the
    /// encoders have to cope with.
    WellFormed,
    /// Anything at all: unanswered and orphaned calls, empty ids, empty
    /// payloads, blocks of any origin anywhere. Encoders only have to
    /// survive it.
    Wild,
}

const TOOL_NAMES: &[&str] = &[
    "get_weather",
    "search_docs",
    "a-b_c",
    "x",
    DOTTED_TOOL,
    LONG_TOOL,
    "9lives",
    "naïve tool",
    "Read",
    // Becomes `mcp_files_read-file` wherever dots and colons are refused,
    // like `DOTTED_TOOL` does.
    "mcp_files_read-file",
];

const TEXTS: &[&str] = &[
    "Hello there.",
    "What is 2 + 2?",
    "Привет, 世界! 🌍",
    "line one\nline two\ttabbed \"quoted\" \\ backslash",
    "<system-reminder>not really</system-reminder>",
];

const BLANK_TEXTS: &[&str] = &["", "   ", "\n\n"];

fn text(rng: &mut Rng, mode: Mode) -> String {
    if rng.chance(3) {
        return "long ".repeat(2_000);
    }
    if mode == Mode::Wild && rng.chance(30) {
        return (*rng.pick(BLANK_TEXTS)).to_string();
    }
    (*rng.pick(TEXTS)).to_string()
}

fn signature(rng: &mut Rng, mode: Mode) -> Option<Signature> {
    if rng.chance(50) {
        return None;
    }
    let origin = *rng.pick(&Protocol::ALL);
    let data = match mode {
        Mode::Wild => *rng.pick(&[
            "QmxvYkRhdGFGb3JUZXN0cw==",
            "",
            "EqQBCkYICBgCIkD",
            "skip_thought_signature_validator",
        ]),
        Mode::WellFormed => *rng.pick(&[
            "QmxvYkRhdGFGb3JUZXN0cw==",
            "EqQBCkYICBgCIkD0",
            "gAAAAABoZW5jcnlwdGVk",
        ]),
    };
    Some(Signature::new(origin, data))
}

/// Media as a decoder files it: the variant follows the media type.
fn well_formed_media(rng: &mut Rng) -> Part {
    let cache_control = rng.chance(10).then(|| json!({"type": "ephemeral"}));
    match rng.below(8) {
        0..=3 => {
            let (source, media_type) = match rng.below(4) {
                0 | 1 => (
                    MediaSource::Base64 {
                        data: PNG_B64.to_string(),
                    },
                    Some("image/png"),
                ),
                2 => (
                    MediaSource::Url {
                        url: "https://example.com/a.png".to_string(),
                    },
                    None,
                ),
                _ => (
                    MediaSource::Url {
                        url: "https://example.com/photo.jpg".to_string(),
                    },
                    Some("image/jpeg"),
                ),
            };
            Part::Image(MediaPart {
                source,
                media_type: media_type.map(str::to_string),
                filename: None,
                detail: rng
                    .chance(30)
                    .then(|| (*rng.pick(&["low", "high", "auto"])).to_string()),
                cache_control,
            })
        }
        4 | 5 => {
            let (source, media_type) = match rng.below(3) {
                0 => (
                    MediaSource::Base64 {
                        data: PDF_B64.to_string(),
                    },
                    "application/pdf",
                ),
                1 => (
                    MediaSource::Base64 {
                        data: "aGVsbG8gd29ybGQ=".to_string(),
                    },
                    "text/plain",
                ),
                _ => (
                    MediaSource::Url {
                        url: "https://example.com/doc.pdf".to_string(),
                    },
                    "application/pdf",
                ),
            };
            Part::Document(MediaPart {
                source,
                media_type: Some(media_type.to_string()),
                filename: rng.chance(50).then(|| "report.pdf".to_string()),
                detail: None,
                cache_control,
            })
        }
        6 => Part::Audio(MediaPart {
            source: MediaSource::Base64 {
                data: "UklGRiQAAABXQVZFZm10IBAAAAABAAEAQB8AAIA+AAACABAAZGF0YQAAAAA=".to_string(),
            },
            media_type: Some((*rng.pick(&["audio/wav", "audio/mpeg"])).to_string()),
            filename: None,
            detail: None,
            cache_control: None,
        }),
        // A clip: the IR files video under `Document`.
        _ => Part::Document(MediaPart {
            source: MediaSource::Url {
                url: "https://example.com/clip.mp4".to_string(),
            },
            media_type: Some("video/mp4".to_string()),
            filename: None,
            detail: None,
            cache_control: None,
        }),
    }
}

fn wild_media(rng: &mut Rng) -> Part {
    let source = match rng.below(4) {
        0 | 1 => MediaSource::Base64 {
            data: (*rng.pick(&[PNG_B64, PDF_B64, "aGVsbG8gd29ybGQ=", ""])).to_string(),
        },
        2 => MediaSource::Url {
            url: (*rng.pick(&[
                "https://example.com/a.png",
                "https://example.com/doc.pdf",
                "data:image/png;base64,iVBORw0KGgo=",
                "gs://bucket/object",
            ]))
            .to_string(),
        },
        _ => MediaSource::FileRef {
            id: "file-abc123".to_string(),
        },
    };
    let media_type = match rng.below(7) {
        0 => None,
        1 => Some("image/png"),
        2 => Some("application/pdf"),
        3 => Some("text/plain"),
        4 => Some("audio/wav"),
        5 => Some("video/mp4"),
        _ => Some("application/msword"),
    };
    let part = MediaPart {
        source,
        media_type: media_type.map(str::to_string),
        filename: rng.chance(30).then(|| "report.pdf".to_string()),
        detail: rng
            .chance(20)
            .then(|| (*rng.pick(&["low", "high", "auto", "original", "ultra"])).to_string()),
        cache_control: rng.chance(10).then(|| json!({"type": "ephemeral"})),
    };
    match rng.below(3) {
        0 => Part::Image(part),
        1 => Part::Document(part),
        _ => Part::Audio(part),
    }
}

fn media(rng: &mut Rng, mode: Mode) -> Part {
    match mode {
        Mode::WellFormed => well_formed_media(rng),
        Mode::Wild => wild_media(rng),
    }
}

/// A block only its own protocol understands. A decoder tags such a block
/// with the protocol it read it from, so a well-formed request only holds
/// blocks of its source protocol, in that protocol's shape.
fn opaque(rng: &mut Rng, source: Protocol, assistant: bool, mode: Mode) -> Part {
    if mode == Mode::Wild {
        let raw = match rng.below(4) {
            0 => {
                json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "x"}})
            }
            1 => json!({"executableCode": {"language": "PYTHON", "code": "print(1)"}}),
            2 => json!({"type": "web_search_call", "id": "ws_1", "status": "completed"}),
            _ => json!("not an object"),
        };
        return Part::Opaque(OpaquePart {
            origin: *rng.pick(&Protocol::ALL),
            raw,
        });
    }
    let raw = match (source, assistant) {
        (Protocol::Anthropic, true) => {
            json!({"type": "server_tool_use", "id": "srvtoolu_01A", "name": "web_search", "input": {"query": "x"}})
        }
        (Protocol::Anthropic, false) => json!({"type": "container_upload", "file_id": "file_011C"}),
        (Protocol::Gemini, true) => {
            json!({"executableCode": {"language": "PYTHON", "code": "print(1)"}})
        }
        (Protocol::Gemini, false) => {
            json!({"videoMetadata": {"fps": 1}, "fileData": {"fileUri": "https://example.com/v.mp4", "mimeType": "video/mp4"}})
        }
        (Protocol::OpenaiResponses, true) => {
            json!({"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "x"}})
        }
        (Protocol::OpenaiResponses, false) => {
            json!({"type": "input_text_extension", "text": "vendor part"})
        }
        (Protocol::OpenaiChat, _) => json!({"type": "vendor_part", "vendor_part": {"k": "v"}}),
    };
    Part::Opaque(OpaquePart {
        origin: source,
        raw,
    })
}

/// A provider-executed tool as a client of `source` declares it.
fn builtin(rng: &mut Rng, source: Protocol, mode: Mode) -> Tool {
    if mode == Mode::Wild {
        return Tool::Builtin(BuiltinTool {
            kind: (*rng.pick(&[
                BuiltinKind::WebSearch,
                BuiltinKind::WebFetch,
                BuiltinKind::CodeExecution,
                BuiltinKind::Other("computer_use".into()),
            ]))
            .clone(),
            origin: *rng.pick(&Protocol::ALL),
            raw: json!({"type": "web_search"}),
        });
    }
    let search = rng.chance(60);
    let (kind, raw) = match (source, search) {
        (Protocol::OpenaiChat, _) => (
            BuiltinKind::WebSearch,
            json!({"web_search_options": {"search_context_size": "low"}}),
        ),
        (Protocol::OpenaiResponses, true) => {
            (BuiltinKind::WebSearch, json!({"type": "web_search"}))
        }
        (Protocol::OpenaiResponses, false) => (
            BuiltinKind::CodeExecution,
            json!({"type": "code_interpreter", "container": {"type": "auto"}}),
        ),
        (Protocol::Anthropic, true) => (
            BuiltinKind::WebSearch,
            json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3}),
        ),
        (Protocol::Anthropic, false) => (
            BuiltinKind::CodeExecution,
            json!({"type": "code_execution_20250825", "name": "code_execution"}),
        ),
        (Protocol::Gemini, true) => (BuiltinKind::WebSearch, json!({"googleSearch": {}})),
        (Protocol::Gemini, false) => (BuiltinKind::CodeExecution, json!({"codeExecution": {}})),
    };
    Tool::Builtin(BuiltinTool {
        kind,
        origin: source,
        raw,
    })
}

fn call_id(rng: &mut Rng, counter: &mut usize, mode: Mode) -> String {
    *counter += 1;
    match rng.below(12) {
        0 if mode == Mode::Wild => String::new(),
        // A client that numbers its calls from one in every turn.
        3 if mode == Mode::Wild => "call_1".to_string(),
        1 => format!("weird id:{counter}/x"),
        // Longer than OpenAI's limits (40 on Chat, 64 on Responses).
        2 => format!("call_{}_{counter}", "z".repeat(90)),
        4 => format!("toolu_01{:022}", *counter),
        5 => format!("fc_{:048x}", *counter as u64 * 0x9E37),
        _ => format!("call_{:024x}", *counter as u64 * 0x9E37),
    }
}

fn arguments(rng: &mut Rng, mode: Mode) -> String {
    let complete: &[&str] = &[
        "{\"location\":\"Paris\"}",
        "{}",
        "",
        "{\"nested\":{\"deep\":[1,{\"x\":null}]},\"n\":1.5e3}",
    ];
    let broken: &[&str] = &[" ", "null", "[1,2]", "\"just a string\"", "{\"location\":"];
    if mode == Mode::Wild && rng.chance(40) {
        return (*rng.pick(broken)).to_string();
    }
    (*rng.pick(complete)).to_string()
}

fn schema(rng: &mut Rng, mode: Mode) -> Value {
    if mode == Mode::Wild {
        match rng.below(8) {
            0 => {
                return json!({"anyOf": [{"type": "object", "properties": {"x": {"type": "string"}}}, {"type": "string"}]});
            }
            1 => return json!(true),
            2 => return json!("not a schema"),
            _ => {}
        }
    }
    match rng.below(6) {
        0 => Value::Null,
        1 => json!({}),
        2 => json!({"type": "object"}),
        3 => {
            json!({"type": "object", "properties": {"location": {"type": "string"}}, "required": ["location"]})
        }
        4 => json!({
            "type": "object",
            "$schema": "http://json-schema.org/draft-07/schema#",
            "properties": {
                "a": {"$ref": "#/$defs/thing"},
                "b": {"type": ["string", "null"]},
                "c": {"anyOf": [{"type": "string"}, {"type": "integer"}]},
                "d": {"type": "array"},
                "e": {"const": 5},
                "f": {"enum": [1, 2, null]},
                "g": true,
                "title": {"type": "string", "title": "Title"}
            },
            "required": ["a", "missing"],
            "$defs": {"thing": {"type": "object"}},
            "additionalProperties": false
        }),
        // The root names no type: fine for Gemini, a 400 on OpenAI and
        // Anthropic unless the encoder supplies it.
        _ => json!({"properties": {"q": {"type": "string", "pattern": "^\\p{L}+$"}}}),
    }
}

fn tool_result(rng: &mut Rng, id: String, name: String, mode: Mode) -> Part {
    let mut content = Vec::new();
    for _ in 0..rng.below(3) {
        content.push(if rng.chance(80) {
            Part::text(text(rng, mode))
        } else {
            media(rng, mode)
        });
    }
    Part::ToolResult(ToolResult {
        call_id: id,
        name: rng.chance(30).then_some(name),
        content,
        is_error: rng.chance(15),
        cache_control: None,
    })
}

/// A conversation as a faithful client keeps it: user and assistant turns
/// alternate, an assistant turn may reason, speak and call tools (in that
/// order), and the user turn after it answers every call.
fn well_formed_messages(rng: &mut Rng, request: &mut Request) {
    let mode = Mode::WellFormed;
    let source = request.source;
    let callable: Vec<(String, ToolCallKind)> = request
        .tools
        .iter()
        .filter_map(|tool| match tool {
            Tool::Function(function) => Some((function.name.clone(), ToolCallKind::Function)),
            Tool::Custom(custom) => Some((custom.name.clone(), ToolCallKind::Custom)),
            Tool::Builtin(_) => None,
        })
        .collect();
    let mut counter = 0usize;

    let mut opening = vec![Part::text((*rng.pick(TEXTS)).to_string())];
    if rng.chance(30) {
        opening.push(media(rng, mode));
    }
    if rng.chance(8) {
        opening.push(opaque(rng, source, false, mode));
    }
    request.messages.push(Message {
        role: Role::User,
        parts: opening,
        name: rng.chance(5).then(|| "alice".to_string()),
    });

    for _ in 0..rng.below(5) {
        let mut parts = Vec::new();
        if rng.chance(40) {
            let signature = signature(rng, mode);
            parts.push(Part::Reasoning(Reasoning {
                id: rng
                    .chance(30)
                    .then(|| (*rng.pick(&["rs_abc", "not-an-rs-id"])).to_string()),
                text: if signature.is_some() && rng.chance(20) {
                    String::new()
                } else {
                    text(rng, mode)
                },
                redacted: signature.is_some() && rng.chance(20),
                signature,
            }));
        }
        if rng.chance(60) {
            parts.push(Part::Text(TextPart {
                text: text(rng, mode),
                signature: if rng.chance(10) {
                    signature(rng, mode)
                } else {
                    None
                },
                ..TextPart::default()
            }));
        }
        if rng.chance(5) {
            parts.push(Part::Refusal(RefusalPart {
                text: text(rng, mode),
            }));
        }
        if rng.chance(8) {
            parts.push(opaque(rng, source, true, mode));
        }
        let mut calls: Vec<(String, String)> = Vec::new();
        if !callable.is_empty() && rng.chance(60) {
            for _ in 0..1 + rng.below(3) {
                let (name, kind) = rng.pick(&callable).clone();
                let id = call_id(rng, &mut counter, mode);
                calls.push((id.clone(), name.clone()));
                parts.push(Part::ToolCall(ToolCall {
                    id,
                    name,
                    arguments: if kind == ToolCallKind::Custom {
                        "SELECT 1;".to_string()
                    } else {
                        arguments(rng, mode)
                    },
                    kind,
                    signature: if rng.chance(30) {
                        signature(rng, mode)
                    } else {
                        None
                    },
                    cache_control: None,
                }));
            }
        }
        // A turn says something: reasoning alone is not an answer.
        if !parts
            .iter()
            .any(|part| matches!(part, Part::Text(_) | Part::ToolCall(_)))
        {
            parts.push(Part::text("Done."));
        }
        request.messages.push(Message::new(Role::Assistant, parts));

        if rng.chance(5) {
            request.messages.push(Message::new(
                Role::System,
                vec![Part::text("Be brief from now on.")],
            ));
        }
        let mut reply: Vec<Part> = calls
            .into_iter()
            .map(|(id, name)| tool_result(rng, id, name, mode))
            .collect();
        if reply.is_empty() || rng.chance(25) {
            reply.push(Part::text((*rng.pick(TEXTS)).to_string()));
        }
        if rng.chance(10) {
            reply.push(media(rng, mode));
        }
        request.messages.push(Message::new(Role::User, reply));
    }
}

fn wild_messages(rng: &mut Rng, request: &mut Request) {
    let mode = Mode::Wild;
    let source = request.source;
    let mut counter = 0usize;
    // Calls of the latest assistant message that nothing answered yet.
    let mut pending: Vec<(String, String)> = Vec::new();
    for _ in 0..rng.below(11) {
        let role = match rng.below(20) {
            0 | 1 => Role::System,
            n if n % 2 == 0 => Role::User,
            _ => Role::Assistant,
        };
        let mut parts = Vec::new();
        match role {
            Role::System => {
                for _ in 0..rng.below(3) {
                    parts.push(Part::text(text(rng, mode)));
                }
            }
            Role::Assistant => {
                pending.clear();
                for _ in 0..rng.below(5) {
                    parts.push(match rng.below(12) {
                        0..=3 => Part::Text(TextPart {
                            text: text(rng, mode),
                            signature: if rng.chance(10) {
                                signature(rng, mode)
                            } else {
                                None
                            },
                            ..TextPart::default()
                        }),
                        4 | 5 => Part::Reasoning(Reasoning {
                            id: rng
                                .chance(30)
                                .then(|| (*rng.pick(&["rs_abc", "not-an-rs-id", ""])).to_string()),
                            text: text(rng, mode),
                            signature: signature(rng, mode),
                            redacted: rng.chance(15),
                        }),
                        6..=8 => {
                            let id = call_id(rng, &mut counter, mode);
                            let name = if rng.chance(3) {
                                String::new()
                            } else {
                                (*rng.pick(TOOL_NAMES)).to_string()
                            };
                            pending.push((id.clone(), name.clone()));
                            Part::ToolCall(ToolCall {
                                id,
                                name,
                                arguments: arguments(rng, mode),
                                kind: if rng.chance(10) {
                                    ToolCallKind::Custom
                                } else {
                                    ToolCallKind::Function
                                },
                                signature: if rng.chance(30) {
                                    signature(rng, mode)
                                } else {
                                    None
                                },
                                cache_control: None,
                            })
                        }
                        9 => Part::Refusal(RefusalPart {
                            text: text(rng, mode),
                        }),
                        10 => media(rng, mode),
                        _ => opaque(rng, source, true, mode),
                    });
                }
            }
            Role::User => {
                // Usually the results of the calls just made, sometimes not
                // all of them, sometimes results nobody asked for.
                while !pending.is_empty() && rng.chance(85) {
                    let (id, name) = pending.remove(0);
                    parts.push(tool_result(rng, id, name, mode));
                }
                if rng.chance(8) {
                    parts.push(Part::ToolResult(ToolResult {
                        call_id: (*rng.pick(&["", "call_never_made", "call_1"])).to_string(),
                        name: None,
                        content: vec![Part::text("orphan output")],
                        is_error: false,
                        cache_control: None,
                    }));
                }
                for _ in 0..rng.below(3) {
                    parts.push(match rng.below(10) {
                        0..=6 => Part::text(text(rng, mode)),
                        7 | 8 => media(rng, mode),
                        _ => opaque(rng, source, false, mode),
                    });
                }
            }
        }
        request.messages.push(Message {
            role,
            parts,
            name: rng.chance(5).then(|| "alice".to_string()),
        });
    }
}

fn random_request(rng: &mut Rng, mode: Mode) -> Request {
    let wild = mode == Mode::Wild;
    let source = *rng.pick(&Protocol::ALL);
    let mut request = Request::new("some-model", source);
    request.stream = rng.chance(50);
    for _ in 0..rng.below(3) {
        request.system.push(if wild && rng.chance(10) {
            media(rng, mode)
        } else {
            Part::text(text(rng, mode))
        });
    }

    let mut declared: Vec<&str> = Vec::new();
    for _ in 0..rng.below(4) {
        let name = *rng.pick(TOOL_NAMES);
        // A vendor refuses two tools of one name; only a wild request has them.
        if wild || !declared.contains(&name) {
            declared.push(name);
        }
    }
    for name in &declared {
        request.tools.push(match rng.below(10) {
            0 => Tool::Custom(CustomTool {
                name: (*name).to_string(),
                description: rng.chance(50).then(|| "A free-form tool.".to_string()),
                format: None,
            }),
            _ => Tool::Function(FunctionTool {
                name: (*name).to_string(),
                description: rng.chance(70).then(|| text(rng, mode)),
                parameters: schema(rng, mode),
                strict: (*rng.pick(&[None, Some(true), Some(false)])),
                cache_control: rng.chance(10).then(|| json!({"type": "ephemeral"})),
            }),
        });
    }
    let mut forced_builtin = None;
    if rng.chance(15) {
        let tool = builtin(rng, source, mode);
        // A Messages client may force a server tool by its name.
        if let Tool::Builtin(spec) = &tool
            && source == Protocol::Anthropic
        {
            forced_builtin = spec
                .raw
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        request.tools.push(tool);
    }
    request.tool_choice = match rng.below(8) {
        _ if !wild && request.tools.is_empty() => None,
        0 => Some(ToolChoice::Auto),
        1 => Some(ToolChoice::None),
        2 => Some(ToolChoice::Required),
        3 if wild => Some(ToolChoice::Tool {
            name: (*rng.pick(TOOL_NAMES)).to_string(),
        }),
        3 | 4 if !declared.is_empty() => Some(ToolChoice::Tool {
            name: (*rng.pick(&declared)).to_string(),
        }),
        5 | 6 if forced_builtin.is_some() => forced_builtin.map(|name| ToolChoice::Tool { name }),
        _ => None,
    };
    request.parallel_tool_calls = *rng.pick(&[None, None, Some(true), Some(false)]);

    match mode {
        Mode::WellFormed => well_formed_messages(rng, &mut request),
        Mode::Wild => wild_messages(rng, &mut request),
    }

    request.max_output_tokens =
        *rng.pick(&[None, None, Some(1), Some(5), Some(4096), Some(1_000_000)]);
    if wild && rng.chance(15) {
        request.max_output_tokens = Some(0);
    }
    request.temperature = *rng.pick(&[None, None, Some(0.0), Some(0.7), Some(1.0), Some(1.9)]);
    request.top_p = *rng.pick(&[None, None, Some(0.1), Some(0.95), Some(1.0)]);
    request.top_k = *rng.pick(&[None, None, Some(1), Some(40)]);
    request.seed = *rng.pick(&[None, None, Some(42), Some(-1), Some(i64::MAX)]);
    request.presence_penalty = *rng.pick(&[None, None, Some(0.0), Some(-2.0), Some(1.5)]);
    request.frequency_penalty = *rng.pick(&[None, None, Some(0.0), Some(0.25)]);
    // Up to five, Gemini's limit; OpenAI takes four.
    for _ in 0..rng.below(9).saturating_sub(3) {
        let stop = *rng.pick(&["STOP", "\n\n", "", " ", "END", "###", "seven"]);
        if wild || (!stop.is_empty() && !request.stop.iter().any(|s| s == stop)) {
            request.stop.push(stop.to_string());
        }
    }
    request.candidate_count = *rng.pick(&[None, None, Some(1), Some(3)]);
    request.reasoning = match rng.below(4) {
        0 => None,
        _ => {
            let depth = *rng.pick(&[
                None,
                Some(Depth::Off),
                Some(Depth::Auto),
                Some(Depth::Level(Effort::Minimal)),
                Some(Depth::Level(Effort::High)),
                Some(Depth::Level(Effort::Max)),
                Some(Depth::Budget(1)),
                Some(Depth::Budget(8192)),
                Some(Depth::Budget(u32::MAX)),
            ]);
            let summary = *rng.pick(&[
                None,
                Some(Summary::Off),
                Some(Summary::Auto),
                Some(Summary::Detailed),
            ]);
            let config = ReasoningConfig { depth, summary };
            (!config.is_empty()).then_some(config)
        }
    };
    request.response_format = match rng.below(8) {
        0 => Some(ResponseFormat::Text),
        1 => Some(ResponseFormat::JsonObject),
        2 => Some(ResponseFormat::JsonSchema {
            name: rng.chance(50).then(|| "person".to_string()),
            description: None,
            schema: json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}),
            strict: *rng.pick(&[None, Some(true)]),
        }),
        _ => None,
    };
    request.user = match rng.below(6) {
        0 => Some("user-1234".to_string()),
        1 => Some("u".repeat(400)),
        2 if wild => Some("  ".to_string()),
        _ => None,
    };
    if rng.chance(15) {
        let mut metadata = Map::new();
        metadata.insert("trace".into(), json!("abc"));
        metadata.insert("attempt".into(), if wild { json!(3) } else { json!("3") });
        request.metadata = Some(metadata);
    }
    // A service tier in the spelling of the vendor the request came from.
    if rng.chance(20) {
        let tiers: &[&str] = match source {
            Protocol::OpenaiChat | Protocol::OpenaiResponses => {
                &["auto", "default", "flex", "priority"]
            }
            Protocol::Anthropic => &["auto", "standard_only"],
            Protocol::Gemini => &["standard", "flex", "priority"],
        };
        request.service_tier = Some((*rng.pick(tiers)).to_string());
    }
    request.store = *rng.pick(&[None, None, None, Some(true), Some(false)]);
    if rng.chance(5) {
        request.prompt_cache_key = Some("cache-key-1".to_string());
    }
    request
}

/// Encodes one generated request for every upstream, with and without
/// knowledge of the target model, the way the gateway does it: the model
/// name is the upstream's and the reasoning depth is fitted first
/// (`docs/DESIGN.md` §2). `check` sees each outcome.
fn encode_everywhere(
    request: &Request,
    seed: u64,
    failures: &mut Failures,
    mut check: impl FnMut(Protocol, &str, &Value, &mut Failures),
) {
    for upstream in Protocol::ALL {
        let caps = known_caps(upstream);
        for (label, ctx) in [("unknown", UpstreamCtx::default()), ("known", caps.ctx())] {
            let context = format!(
                "seed {seed} / {} -> {} / {label}",
                short(request.source),
                short(upstream)
            );
            let mut request = request.clone();
            request.model = upstream_model(upstream).to_string();
            fit_reasoning(&mut request, ctx.thinking, upstream);
            let outcome = no_panic(&context, || {
                let _ = codec(upstream).encode_count_request(&request, &ctx);
                codec(upstream).encode_request(&request, &ctx)
            });
            match outcome {
                Err(panic) => failures.push(&context, panic),
                Ok(Err(error)) => {
                    failures.push(&context, format!("encode_request failed: {error}"))
                }
                Ok(Ok(body)) => check(upstream, &context, &body, failures),
            }
        }
    }
}

#[test]
fn wild_requests_never_panic_an_encoder() {
    let mut failures = Failures::default();
    for seed in 1..=600u64 {
        let mut rng = Rng::new(seed ^ 0x57494C44);
        let request = random_request(&mut rng, Mode::Wild);
        encode_everywhere(
            &request,
            seed,
            &mut failures,
            |_, context, body, failures| {
                failures.check(
                    context,
                    body.is_object(),
                    "the encoded body is not a JSON object",
                );
            },
        );
    }
    failures.finish_grouped("wild requests");
}

#[test]
fn random_requests_encode_to_valid_bodies() {
    let mut failures = Failures::default();
    for seed in 1..=600u64 {
        let mut rng = Rng::new(seed);
        let request = random_request(&mut rng, Mode::WellFormed);
        let source = request.source;
        encode_everywhere(
            &request,
            seed,
            &mut failures,
            |upstream, context, body, failures| {
                // A request re-encoded for its own protocol replays what the
                // client wrote (its tool names, its schemas, its provider
                // blocks): what the vendor would refuse there, it would have
                // refused from the client directly.
                if upstream != source {
                    failures.report(context, validate_translated_request(source, upstream, body));
                }
            },
        );
    }
    failures.finish_grouped("random requests");
}

// ---------------------------------------------------------------------------
// Random canonical responses
// ---------------------------------------------------------------------------

/// A response as some upstream's decoder yields it: every blob and every
/// provider block in it was issued by that one upstream.
fn random_response(rng: &mut Rng) -> Response {
    let upstream = *rng.pick(&Protocol::ALL);
    let blob = |rng: &mut Rng| {
        rng.chance(50).then(|| {
            Signature::new(
                upstream,
                *rng.pick(&[
                    "QmxvYkRhdGFGb3JUZXN0cw==",
                    "EqQBCkYICBgCIkD0",
                    "gAAAAABoZW5jcnlwdGVk",
                ]),
            )
        })
    };
    let mut response = Response::new(
        (*rng.pick(&[
            "",
            "chatcmpl-abc",
            "msg_01abc",
            "resp_abc",
            "gem id/with spaces",
        ]))
        .to_string(),
        (*rng.pick(&["", "upstream-model"])).to_string(),
    );
    response.created = *rng.pick(&[0, 1_759_400_000]);
    let finish = (*rng.pick(&[
        FinishReason::Stop,
        FinishReason::Stop,
        FinishReason::Length,
        FinishReason::ContentFilter,
        FinishReason::Refusal,
        FinishReason::PauseTurn,
        FinishReason::ContextWindow,
        FinishReason::Other("WEIRD".into()),
    ]))
    .clone();
    let mut counter = 0usize;
    let mut has_calls = false;
    for _ in 0..rng.below(6) {
        response.parts.push(match rng.below(10) {
            0..=3 => Part::Text(TextPart {
                text: text(rng, Mode::Wild),
                signature: if rng.chance(10) { blob(rng) } else { None },
                ..TextPart::default()
            }),
            4 | 5 => Part::Reasoning(Reasoning {
                id: rng.chance(30).then(|| {
                    counter += 1;
                    format!("rs_abc{counter}")
                }),
                text: text(rng, Mode::Wild),
                signature: blob(rng),
                redacted: rng.chance(15),
            }),
            6 | 7 => {
                has_calls = true;
                counter += 1;
                Part::ToolCall(ToolCall {
                    id: match rng.below(4) {
                        0 => String::new(),
                        1 => format!("weird id:{counter}"),
                        _ => format!("call_{counter:024}"),
                    },
                    name: (*rng.pick(&[
                        "get_weather",
                        "search_docs",
                        "_9lives",
                        "undeclared.tool",
                    ]))
                    .to_string(),
                    // A complete call carries a complete JSON document (or
                    // nothing); a cut-off one only ends a `Length` response.
                    arguments: (*rng.pick(&[
                        "{\"location\":\"Paris\"}",
                        "{}",
                        "",
                        "{\"a\":[1,2,{\"b\":null}]}",
                    ]))
                    .to_string(),
                    kind: ToolCallKind::Function,
                    signature: if rng.chance(30) { blob(rng) } else { None },
                    cache_control: None,
                })
            }
            8 => Part::Refusal(RefusalPart {
                text: text(rng, Mode::Wild),
            }),
            // One provider block at most: a second identical one would be a
            // duplicate item id, which no upstream sends.
            _ if response
                .parts
                .iter()
                .any(|part| matches!(part, Part::Opaque(_))) =>
            {
                Part::text("Searched once.")
            }
            _ => opaque(rng, upstream, true, Mode::WellFormed),
        });
    }
    response.finish = if has_calls && finish == FinishReason::Stop {
        FinishReason::ToolCalls
    } else {
        finish
    };
    if response.finish == FinishReason::Length
        && rng.chance(50)
        && let Some(Part::ToolCall(call)) = response.parts.last_mut()
    {
        call.arguments = "{\"location\":\"Par".to_string();
    }
    response.stop_sequence =
        (response.finish == FinishReason::Stop && rng.chance(20)).then(|| "STOP".to_string());
    let output = rng.below(500) as u64;
    response.usage = Usage {
        input_tokens: rng.below(1000) as u64,
        cache_read_tokens: *rng.pick(&[0, 0, 800]),
        cache_write_tokens: *rng.pick(&[0, 0, 150]),
        output_tokens: output,
        reasoning_tokens: if rng.chance(30) { output / 2 } else { 0 },
    };
    response
}

/// Splits the deltas of a replayed response into small fragments, as a real
/// upstream stream arrives.
fn fragmented(events: Vec<StreamEvent>, rng: &mut Rng) -> Vec<StreamEvent> {
    fn pieces(text: &str, rng: &mut Rng) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        let mut out = Vec::new();
        let mut at = 0;
        while at < chars.len() {
            let take = 1 + rng.below(40);
            out.push(chars[at..(at + take).min(chars.len())].iter().collect());
            at += take;
        }
        out
    }
    let mut out = Vec::new();
    for event in events {
        match event {
            StreamEvent::TextDelta { index, text } => out.extend(
                pieces(&text, rng)
                    .into_iter()
                    .map(|text| StreamEvent::TextDelta { index, text }),
            ),
            StreamEvent::ReasoningDelta { index, text } => out.extend(
                pieces(&text, rng)
                    .into_iter()
                    .map(|text| StreamEvent::ReasoningDelta { index, text }),
            ),
            StreamEvent::ToolArgsDelta { index, fragment } => out.extend(
                pieces(&fragment, rng)
                    .into_iter()
                    .map(|fragment| StreamEvent::ToolArgsDelta { index, fragment }),
            ),
            other => out.push(other),
        }
    }
    out
}

#[test]
fn random_responses_encode_to_valid_bodies_and_streams() {
    let mut failures = Failures::default();
    for seed in 1..=400u64 {
        let mut rng = Rng::new(seed ^ 0xABCD);
        let response = random_response(&mut rng);
        let events = fragmented(response_to_events(&response), &mut rng);
        for client in Protocol::ALL {
            let context = format!("seed {seed} / {} client", short(client));
            let ctx = client_ctx(client, &scenarios::tool_request(client));
            match no_panic(&context, || codec(client).encode_response(&response, &ctx)) {
                Err(panic) => failures.push(&context, panic),
                Ok(Err(error)) => {
                    failures.push(&context, format!("encode_response failed: {error}"))
                }
                Ok(Ok(body)) => {
                    failures.report(&context, validate_response(client, &body));
                    failures.check(
                        &context,
                        client == Protocol::Gemini || body["model"] == client_model(client),
                        "the client's model name is not reported back",
                    );
                    // The client's own decoder reads what it was sent.
                    if let Err(error) = codec(client).decode_response(&body) {
                        failures.push(
                            &context,
                            format!("the encoded response does not decode again: {error}"),
                        );
                    }
                }
            }
            let context = format!("seed {seed} / {} client stream", short(client));
            match no_panic(&context, || {
                over_the_wire(&encode_stream(client, &ctx, &events))
            }) {
                Err(panic) => failures.push(&context, panic),
                Ok(wire) => {
                    failures.report(&context, validate_stream(client, &wire));
                    if let Err(panic) = no_panic(&context, || decode_stream(client, &wire)) {
                        failures.push(&context, panic);
                    }
                }
            }
        }
    }
    failures.finish_grouped("random responses");
}
