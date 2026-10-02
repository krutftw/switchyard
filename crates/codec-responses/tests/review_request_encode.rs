//! Regression tests for request encoding defects found in review: reasoning
//! items replayed to a model known not to reason (R2) and translated
//! requests opting in to vendor-side storage (R9), plus related limits. The
//! `review_*` tests are the reviewer's originals.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::ir::{FunctionTool, Message, Part, Reasoning, Request, Role, Signature, Tool};
use switchyard_core::reasoning::ModelThinking;
use switchyard_core::{Codec, Protocol, UpstreamCtx};

const P: Protocol = Protocol::OpenaiResponses;

fn tools_for(source: Protocol, parameters: Value) -> Value {
    let mut request = Request::new("gpt-4.1", source);
    request.messages = vec![Message::user_text("hello")];
    request.tools = vec![Tool::Function(FunctionTool {
        name: "f".into(),
        description: None,
        parameters,
        strict: None,
        cache_control: None,
    })];
    ResponsesCodec
        .encode_request(&request, &UpstreamCtx::default())
        .expect("encodes")["tools"]
        .clone()
}

/// Tool schemas written for other protocols get the repairs that keep this
/// API from rejecting the whole request (99 §U5, 08 §8.3): an object root
/// with `properties`, no dialect markers, no patterns its validator cannot
/// compile. What the tool accepts is unchanged.
#[test]
fn tool_schemas_from_other_protocols_are_made_acceptable() {
    for source in [Protocol::Anthropic, Protocol::Gemini, Protocol::OpenaiChat] {
        // The usual schema of a tool that takes no arguments.
        assert_eq!(
            tools_for(source, json!({"type": "object"})),
            json!([{"type": "function", "name": "f", "parameters": {"type": "object", "properties": {}}, "strict": false}]),
            "{source}"
        );
        assert_eq!(
            tools_for(source, json!({}))[0]["parameters"],
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            tools_for(source, Value::Null)[0]["parameters"],
            json!({"type": "object", "properties": {}})
        );
        // What a schema generator leaves behind.
        assert_eq!(
            tools_for(
                source,
                json!({
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "type": "object",
                    "properties": {"name": {"type": "string", "pattern": "^\\p{L}+$", "minLength": 1}},
                    "required": ["name"],
                    "additionalProperties": false
                })
            )[0]["parameters"],
            json!({
                "type": "object",
                "properties": {"name": {"type": "string", "minLength": 1}},
                "required": ["name"],
                "additionalProperties": false
            })
        );
    }
}

/// A Responses client wrote its schema for this API; it is forwarded as is.
#[test]
fn a_responses_clients_tool_schema_is_forwarded_as_written() {
    let schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {"name": {"type": "string", "pattern": "^\\p{L}+$"}}
    });
    assert_eq!(
        tools_for(P, schema.clone()),
        json!([{"type": "function", "name": "f", "parameters": schema}])
    );
    assert_eq!(
        tools_for(P, json!({"type": "object"}))[0]["parameters"],
        json!({"type": "object"})
    );
}

/// A conversation that started on a reasoning model: the assistant turn
/// holds a reasoning part signed by this vendor family, then a tool call.
fn conversation(source: Protocol) -> Request {
    let mut request = Request::new("gpt-4.1", source);
    request.messages = vec![
        Message::user_text("What is the weather in Paris?"),
        Message::new(
            Role::Assistant,
            vec![
                Part::Reasoning(Reasoning {
                    id: Some("rs_0a1b".into()),
                    text: "I should call the weather tool.".into(),
                    signature: Some(Signature::new(P, "gAAAAABencrypted")),
                    redacted: false,
                }),
                Part::tool_call("call_1", "get_weather", "{\"city\":\"Paris\"}"),
            ],
        ),
        Message::new(Role::User, vec![Part::tool_result_text("call_1", "18C")]),
    ];
    request
}

fn item_types(body: &Value) -> Vec<&str> {
    body["input"]
        .as_array()
        .expect("input array")
        .iter()
        .map(|item| item["type"].as_str().unwrap_or(""))
        .collect()
}

/// `UpstreamCtx::thinking == ModelThinking::Unsupported` means "the model is
/// known not to reason: reasoning settings must be removed" (core
/// `reasoning.rs`), and DESIGN §3 requires `encode_request` to "keep the body
/// valid". The encoder honours that for `reasoning` and `include`, but still
/// replays `reasoning` *input items*. The vendor rejects those on a
/// non-reasoning model with HTTP 400 ("Reasoning input items can only be
/// provided to a reasoning or computer use model"), so every follow-up turn
/// fails once a conversation moves from a reasoning model to a plain one
/// (alias failover, a client switching models mid-session).
#[test]
fn review_reasoning_items_are_not_replayed_to_a_model_known_not_to_reason() {
    for source in [Protocol::Anthropic, P] {
        let ctx = UpstreamCtx {
            thinking: ModelThinking::Unsupported,
            ..UpstreamCtx::default()
        };
        let body = ResponsesCodec
            .encode_request(&conversation(source), &ctx)
            .expect("encodes");
        assert_eq!(
            body.get("reasoning"),
            None,
            "settings are stripped (passes)"
        );
        assert_eq!(body.get("include"), None, "include is stripped (passes)");
        assert_eq!(
            item_types(&body),
            vec!["message", "function_call", "function_call_output"],
            "source {source}: a `reasoning` input item was sent to a model that cannot reason"
        );
    }
}

/// The token-counting body is derived from the same input list and has the
/// same problem.
#[test]
fn review_count_request_does_not_replay_reasoning_items_to_a_non_reasoning_model() {
    let ctx = UpstreamCtx {
        thinking: ModelThinking::Unsupported,
        ..UpstreamCtx::default()
    };
    let body = ResponsesCodec
        .encode_count_request(&conversation(Protocol::Anthropic), &ctx)
        .expect("count body");
    assert_eq!(
        item_types(&body),
        vec!["message", "function_call", "function_call_output"]
    );
}

/// Control: with a reasoning-capable (or unknown) model the item is replayed.
/// Passes today; guards the fix against dropping too much.
#[test]
fn review_reasoning_items_are_still_replayed_to_reasoning_models() {
    let body = ResponsesCodec
        .encode_request(&conversation(Protocol::Anthropic), &UpstreamCtx::default())
        .expect("encodes");
    assert_eq!(
        item_types(&body),
        vec![
            "message",
            "reasoning",
            "function_call",
            "function_call_output"
        ]
    );
    assert_eq!(
        body["input"][1]["encrypted_content"],
        json!("gAAAAABencrypted")
    );
}

/// Responses is the only protocol whose responses are *stored at the vendor
/// by default* (`store` defaults to true, 15-api-research §4.1). A Chat
/// Completions request without `store` means "do not store" (its default is
/// false) and Anthropic / Gemini requests have no stored-response concept at
/// all. The encoder already spells out the analogous default flip for
/// function tools (`strict: false` for non-Responses sources); `store` needs
/// the same treatment, otherwise every translated request to a model that is
/// not known to reason silently becomes a stored, retrievable response on the
/// upstream account. (For reasoning models the encoder already sends
/// `store: false`, so the retention behaviour currently depends on the
/// model's thinking metadata.)
#[test]
fn review_translated_requests_do_not_opt_in_to_vendor_side_storage() {
    for source in [Protocol::OpenaiChat, Protocol::Anthropic, Protocol::Gemini] {
        for thinking in [ModelThinking::Unknown, ModelThinking::Unsupported] {
            let mut request = Request::new("gpt-4.1", source);
            request.messages = vec![Message::user_text("hello")];
            let ctx = UpstreamCtx {
                thinking,
                ..UpstreamCtx::default()
            };
            let body = ResponsesCodec.encode_request(&request, &ctx).unwrap();
            assert_eq!(
                body.get("store"),
                Some(&json!(false)),
                "source {source}, {thinking:?}: the client never asked for the response to be stored"
            );
        }
    }

    // A client that did ask keeps its choice. Passes today.
    let mut request = Request::new("gpt-4.1", Protocol::OpenaiChat);
    request.messages = vec![Message::user_text("hello")];
    request.store = Some(true);
    let body = ResponsesCodec
        .encode_request(&request, &UpstreamCtx::default())
        .unwrap();
    assert_eq!(body.get("store"), Some(&json!(true)));
}

/// The other half of the rule: a Responses client that says nothing about
/// `store` asked for this API's own default, so nothing is written for it.
#[test]
fn a_responses_clients_silence_about_store_is_left_alone() {
    for thinking in [ModelThinking::Unknown, ModelThinking::Unsupported] {
        let mut request = Request::new("gpt-4.1", P);
        request.messages = vec![Message::user_text("hello")];
        let ctx = UpstreamCtx {
            thinking,
            ..UpstreamCtx::default()
        };
        let body = ResponsesCodec.encode_request(&request, &ctx).unwrap();
        assert_eq!(body.get("store"), None, "{thinking:?}");
    }
}

/// The count body has no `store` (the endpoint does not take it) whatever
/// the source protocol.
#[test]
fn count_request_never_carries_store() {
    let mut request = Request::new("gpt-4.1", Protocol::Anthropic);
    request.messages = vec![Message::user_text("hello")];
    let body = ResponsesCodec
        .encode_count_request(&request, &UpstreamCtx::default())
        .unwrap();
    assert_eq!(
        body,
        json!({"model": "gpt-4.1", "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}
        ]})
    );
}

/// A turn whose reasoning is dropped for a non-reasoning model must not lose
/// anything else, and unsigned / foreign reasoning stays dropped for every
/// kind of model.
#[test]
fn dropping_reasoning_for_a_plain_model_keeps_the_rest_of_the_turn() {
    let mut request = conversation(Protocol::Anthropic);
    request.messages[1]
        .parts
        .insert(1, Part::text("Let me check."));
    let ctx = UpstreamCtx {
        thinking: ModelThinking::Unsupported,
        ..UpstreamCtx::default()
    };
    let body = ResponsesCodec.encode_request(&request, &ctx).unwrap();
    assert_eq!(
        body["input"],
        json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "What is the weather in Paris?"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Let me check."}]},
            {"type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "18C"}
        ])
    );
    assert_eq!(body.get("include"), None);
    assert_eq!(body["store"], json!(false));
}

/// A reasoning item's id is optional next to its blob, and the vendor only
/// takes ids in its own `rs_…` shape (99 §U5): anything else is left out
/// rather than sent or renamed (the blob is bound to the original id).
#[test]
fn reasoning_item_ids_are_only_sent_in_the_vendors_shape() {
    let encode_with_id = |id: Option<&str>| {
        let mut request = conversation(P);
        if let Part::Reasoning(reasoning) = &mut request.messages[1].parts[0] {
            reasoning.id = id.map(str::to_string);
        }
        let body = ResponsesCodec
            .encode_request(&request, &UpstreamCtx::default())
            .unwrap();
        body["input"][1].clone()
    };
    let expected = |id: Option<&str>| {
        let mut item = json!({"type": "reasoning"});
        if let Some(id) = id {
            item["id"] = json!(id);
        }
        item["summary"] =
            json!([{"type": "summary_text", "text": "I should call the weather tool."}]);
        item["encrypted_content"] = json!("gAAAAABencrypted");
        item
    };
    assert_eq!(encode_with_id(Some("rs_0a1b")), expected(Some("rs_0a1b")));
    assert_eq!(encode_with_id(None), expected(None));
    assert_eq!(encode_with_id(Some("")), expected(None));
    assert_eq!(encode_with_id(Some("thinking_block_3")), expected(None));
    let too_long = format!("rs_{}", "a".repeat(62));
    assert_eq!(encode_with_id(Some(&too_long)), expected(None));
    let longest = format!("rs_{}", "a".repeat(61));
    assert_eq!(encode_with_id(Some(&longest)), expected(Some(&longest)));
}

/// Clients of other protocols must always state an output limit and often
/// ask for more than the target model can produce; the vendor answers that
/// with a 400 instead of capping. The limit is kept within the model's.
#[test]
fn max_output_tokens_is_kept_within_the_models_limit() {
    let encode_limit = |limit: u64, ceiling: Option<u64>| {
        let mut request = Request::new("gpt-4.1", Protocol::Anthropic);
        request.messages = vec![Message::user_text("hello")];
        request.max_output_tokens = Some(limit);
        let ctx = UpstreamCtx {
            max_output_tokens: ceiling,
            ..UpstreamCtx::default()
        };
        ResponsesCodec.encode_request(&request, &ctx).unwrap()["max_output_tokens"].clone()
    };
    assert_eq!(encode_limit(64_000, Some(32_768)), json!(32_768));
    assert_eq!(encode_limit(4_096, Some(32_768)), json!(4_096));
    assert_eq!(encode_limit(64_000, None), json!(64_000));
    assert_eq!(encode_limit(1, Some(32_768)), json!(16));
    // A nonsensical ceiling is ignored rather than turned into a limit the
    // vendor would reject.
    assert_eq!(encode_limit(64_000, Some(0)), json!(64_000));

    // Nothing is invented when the client gave no limit.
    let mut request = Request::new("gpt-4.1", Protocol::Anthropic);
    request.messages = vec![Message::user_text("hello")];
    let ctx = UpstreamCtx {
        max_output_tokens: Some(32_768),
        ..UpstreamCtx::default()
    };
    let body = ResponsesCodec.encode_request(&request, &ctx).unwrap();
    assert_eq!(body.get("max_output_tokens"), None);
}
