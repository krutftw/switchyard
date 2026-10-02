//! A local, deterministic estimate of a request's input tokens.
//!
//! Token counting normally goes to the upstream's own counting endpoint
//! (Anthropic `count_tokens`, Gemini `countTokens`, OpenAI Responses
//! `input_tokens`). When the serving upstream has none, the gateway answers
//! the client's count request from [`estimate_tokens`] instead.
//!
//! **This is an estimate, not a tokenizer.** It applies the usual rule of
//! thumb of four characters per token and is typically within a few tens of
//! percent for English prose and code; it under-counts scripts that tokenize
//! to more than one token per four characters (CJK, emoji). It never looks at
//! the model, so the same request always yields the same number.

use serde_json::Value;
use switchyard_core::ir::{Part, Request, ResponseFormat, Tool};

/// Characters assumed per token.
pub const CHARS_PER_TOKEN: u64 = 4;

/// Flat token cost charged for every media part (image, audio, document),
/// whatever its size or encoding. Vendors bill media by resolution, duration
/// or page count — none of which can be known without decoding the payload —
/// so a round figure of the right order of magnitude for a typical image is
/// used.
pub const MEDIA_TOKENS: u64 = 1_000;

/// Estimates the input tokens of `req`.
///
/// `ceil(characters / 4)` over
///
/// * the leading system instructions,
/// * every message: text, refusals, reasoning text, tool-call names and
///   arguments, tool results (recursively), provider-specific blocks (as
///   their JSON text), participant names,
/// * tool definitions: name, description and the parameter schema / format /
///   raw declaration as JSON text,
/// * a JSON-schema response format (its name, description and schema),
///
/// plus [`MEDIA_TOKENS`] for each image, audio clip or document.
///
/// Opaque signatures and encrypted reasoning payloads are not counted: they
/// are not prompt text. Characters are Unicode scalar values, not bytes. An
/// empty request estimates to `0`.
pub fn estimate_tokens(req: &Request) -> u64 {
    let mut tally = Tally::default();

    tally.parts(&req.system);
    for message in &req.messages {
        if let Some(name) = &message.name {
            tally.text(name);
        }
        tally.parts(&message.parts);
    }

    for tool in &req.tools {
        match tool {
            Tool::Function(function) => {
                tally.text(&function.name);
                if let Some(description) = &function.description {
                    tally.text(description);
                }
                if !function.parameters.is_null() {
                    tally.json(&function.parameters);
                }
            }
            Tool::Custom(custom) => {
                tally.text(&custom.name);
                if let Some(description) = &custom.description {
                    tally.text(description);
                }
                if let Some(format) = &custom.format {
                    tally.json(format);
                }
            }
            Tool::Builtin(builtin) => tally.json(&builtin.raw),
        }
    }

    if let Some(ResponseFormat::JsonSchema {
        name,
        description,
        schema,
        ..
    }) = &req.response_format
    {
        if let Some(name) = name {
            tally.text(name);
        }
        if let Some(description) = description {
            tally.text(description);
        }
        tally.json(schema);
    }

    tally.tokens()
}

/// Running totals of what has been seen.
#[derive(Default)]
struct Tally {
    chars: u64,
    media: u64,
}

impl Tally {
    fn text(&mut self, text: &str) {
        self.chars = self.chars.saturating_add(text.chars().count() as u64);
    }

    fn json(&mut self, value: &Value) {
        // `Value`'s `Display` is compact JSON and cannot fail.
        self.text(&value.to_string());
    }

    fn parts(&mut self, parts: &[Part]) {
        for part in parts {
            match part {
                Part::Text(text) => self.text(&text.text),
                Part::Image(_) | Part::Audio(_) | Part::Document(_) => {
                    self.media = self.media.saturating_add(1);
                }
                Part::ToolCall(call) => {
                    self.text(&call.name);
                    self.text(&call.arguments);
                }
                Part::ToolResult(result) => self.parts(&result.content),
                Part::Reasoning(reasoning) => self.text(&reasoning.text),
                Part::Refusal(refusal) => self.text(&refusal.text),
                Part::Opaque(opaque) => self.json(&opaque.raw),
            }
        }
    }

    fn tokens(&self) -> u64 {
        self.chars
            .div_ceil(CHARS_PER_TOKEN)
            .saturating_add(self.media.saturating_mul(MEDIA_TOKENS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use switchyard_core::ir::{
        BuiltinKind, BuiltinTool, CustomTool, FunctionTool, MediaPart, Message, OpaquePart,
        Reasoning, RefusalPart, Role, Signature, ToolResult,
    };
    use switchyard_core::protocol::Protocol;

    fn request() -> Request {
        Request::new("m", Protocol::OpenaiChat)
    }

    fn with_user_text(text: &str) -> Request {
        let mut req = request();
        req.messages.push(Message::user_text(text));
        req
    }

    #[test]
    fn empty_request_is_zero() {
        assert_eq!(estimate_tokens(&request()), 0);
    }

    #[test]
    fn characters_are_divided_by_four_rounding_up() {
        assert_eq!(estimate_tokens(&with_user_text("")), 0);
        assert_eq!(estimate_tokens(&with_user_text("a")), 1);
        assert_eq!(estimate_tokens(&with_user_text("abcd")), 1);
        assert_eq!(estimate_tokens(&with_user_text("abcde")), 2);
        assert_eq!(estimate_tokens(&with_user_text(&"x".repeat(400))), 100);
        assert_eq!(estimate_tokens(&with_user_text(&"x".repeat(401))), 101);
    }

    #[test]
    fn characters_not_bytes() {
        // Four scalar values, twelve bytes.
        assert_eq!(estimate_tokens(&with_user_text("日本語字")), 1);
        assert_eq!(estimate_tokens(&with_user_text("ééééé")), 2);
    }

    #[test]
    fn rounding_happens_once_over_the_whole_request() {
        // Three one-character messages are three characters, not three tokens.
        let mut req = request();
        for _ in 0..3 {
            req.messages.push(Message::user_text("a"));
        }
        assert_eq!(estimate_tokens(&req), 1);
        req.messages.push(Message::user_text("ab"));
        assert_eq!(estimate_tokens(&req), 2);
    }

    #[test]
    fn system_and_messages_are_both_counted() {
        let mut req = request();
        req.system.push(Part::text("12345678"));
        req.messages.push(Message::user_text("1234"));
        req.messages.push(Message::assistant_text("1234"));
        assert_eq!(estimate_tokens(&req), 4);
    }

    #[test]
    fn participant_names_are_counted() {
        let mut req = request();
        let mut message = Message::user_text("1234");
        message.name = Some("abcd".into());
        req.messages.push(message);
        assert_eq!(estimate_tokens(&req), 2);
    }

    #[test]
    fn images_cost_a_flat_thousand() {
        let mut req = request();
        req.messages.push(Message::new(
            Role::User,
            vec![
                Part::text("what is this?!"),
                Part::Image(MediaPart::base64("image/png", "A".repeat(100_000))),
            ],
        ));
        // 14 chars -> 4 tokens, plus the image; the base64 size is irrelevant.
        assert_eq!(estimate_tokens(&req), 1_004);
        req.messages[0]
            .parts
            .push(Part::Image(MediaPart::url("https://example.com/cat.png")));
        assert_eq!(estimate_tokens(&req), 2_004);
    }

    #[test]
    fn audio_and_documents_cost_the_same_flat_amount() {
        let mut req = request();
        req.messages.push(Message::new(
            Role::User,
            vec![
                Part::Audio(MediaPart::base64("audio/wav", "AAAA")),
                Part::Document(MediaPart::base64("application/pdf", "AAAA")),
            ],
        ));
        assert_eq!(estimate_tokens(&req), 2 * MEDIA_TOKENS);
    }

    #[test]
    fn tool_calls_count_name_and_arguments() {
        let mut req = request();
        req.messages.push(Message::new(
            Role::Assistant,
            vec![Part::tool_call("call_ignored", "look", "{\"q\":\"abcd\"}")],
        ));
        // "look" (4) + {"q":"abcd"} (12) = 16 chars; the call id is not text.
        assert_eq!(estimate_tokens(&req), 4);
    }

    #[test]
    fn tool_results_are_counted_recursively() {
        let mut req = request();
        req.messages.push(Message::new(
            Role::User,
            vec![Part::ToolResult(ToolResult {
                call_id: "c1".into(),
                name: None,
                content: vec![
                    Part::text("12345678"),
                    Part::Image(MediaPart::url("https://example.com/plot.png")),
                ],
                is_error: false,
                cache_control: None,
            })],
        ));
        assert_eq!(estimate_tokens(&req), 2 + MEDIA_TOKENS);
    }

    #[test]
    fn reasoning_text_counts_but_signatures_do_not() {
        let mut req = request();
        req.messages.push(Message::new(
            Role::Assistant,
            vec![Part::Reasoning(Reasoning {
                id: Some("rs_1".into()),
                text: "12345678".into(),
                signature: Some(Signature::new(Protocol::Anthropic, "S".repeat(4_000))),
                redacted: false,
            })],
        ));
        assert_eq!(estimate_tokens(&req), 2);
    }

    #[test]
    fn refusals_and_opaque_blocks_are_counted() {
        let mut req = request();
        req.messages.push(Message::new(
            Role::Assistant,
            vec![
                Part::Refusal(RefusalPart {
                    text: "1234".into(),
                }),
                Part::Opaque(OpaquePart {
                    origin: Protocol::Anthropic,
                    // Compact JSON: {"a":1} = 7 characters.
                    raw: json!({"a": 1}),
                }),
            ],
        ));
        // 4 + 7 = 11 chars.
        assert_eq!(estimate_tokens(&req), 3);
    }

    #[test]
    fn function_tools_count_name_description_and_schema() {
        let mut req = request();
        req.tools.push(Tool::Function(FunctionTool {
            name: "abcd".into(),
            description: Some("12345678".into()),
            // {"type":"object"} = 17 characters.
            parameters: json!({"type": "object"}),
            strict: None,
            cache_control: None,
        }));
        // 4 + 8 + 17 = 29 chars.
        assert_eq!(estimate_tokens(&req), 8);
    }

    #[test]
    fn a_function_tool_without_parameters_counts_no_schema() {
        let mut req = request();
        req.tools.push(Tool::Function(FunctionTool {
            name: "abcd".into(),
            description: None,
            parameters: Value::Null,
            strict: None,
            cache_control: None,
        }));
        assert_eq!(estimate_tokens(&req), 1);
    }

    #[test]
    fn custom_and_builtin_tools_are_counted() {
        let mut req = request();
        req.tools.push(Tool::Custom(CustomTool {
            name: "abcd".into(),
            description: Some("1234".into()),
            // {"type":"text"} = 15 characters.
            format: Some(json!({"type": "text"})),
        }));
        // 4 + 4 + 15 = 23 chars.
        assert_eq!(estimate_tokens(&req), 6);

        let mut req = request();
        req.tools.push(Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::OpenaiResponses,
            // {"type":"web_search"} = 21 characters.
            raw: json!({"type": "web_search"}),
        }));
        assert_eq!(estimate_tokens(&req), 6);
    }

    #[test]
    fn json_schema_response_format_is_counted() {
        let mut req = request();
        req.response_format = Some(ResponseFormat::JsonSchema {
            name: Some("abcd".into()),
            description: None,
            // {"type":"object"} = 17 characters.
            schema: json!({"type": "object"}),
            strict: Some(true),
        });
        // 4 + 17 = 21 chars.
        assert_eq!(estimate_tokens(&req), 6);
        req.response_format = Some(ResponseFormat::JsonObject);
        assert_eq!(estimate_tokens(&req), 0);
    }

    #[test]
    fn the_estimate_is_deterministic() {
        let mut req = with_user_text("The quick brown fox jumps over the lazy dog.");
        req.system.push(Part::text("You are terse."));
        req.tools.push(Tool::Function(FunctionTool {
            name: "lookup".into(),
            description: Some("Look something up".into()),
            parameters: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
            strict: None,
            cache_control: None,
        }));
        let first = estimate_tokens(&req);
        assert!(first > 0);
        for _ in 0..5 {
            assert_eq!(estimate_tokens(&req.clone()), first);
        }
    }

    #[test]
    fn fields_that_are_not_prompt_text_do_not_count() {
        let mut req = with_user_text("1234");
        let baseline = estimate_tokens(&req);
        req.model = "a-very-long-model-name-that-is-not-part-of-the-prompt".into();
        req.user = Some("user-identifier".into());
        req.stop = vec!["STOP".into()];
        req.max_output_tokens = Some(4096);
        req.temperature = Some(0.5);
        assert_eq!(estimate_tokens(&req), baseline);
    }

    #[test]
    fn a_plausible_conversation() {
        let mut req = request();
        req.system.push(Part::text("You are a helpful assistant."));
        req.messages
            .push(Message::user_text("What's the weather in Paris?"));
        req.messages.push(Message::new(
            Role::Assistant,
            vec![Part::tool_call("c1", "get_weather", "{\"city\":\"Paris\"}")],
        ));
        req.messages.push(Message::new(
            Role::User,
            vec![Part::tool_result_text("c1", "18C, cloudy")],
        ));
        // 28 + 28 + (11 + 16) + 11 = 94 characters.
        assert_eq!(estimate_tokens(&req), 24);
    }
}
