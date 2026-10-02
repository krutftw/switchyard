//! Raw-body helpers used by same-protocol passthrough, model listings and
//! token counting.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_anthropic::AnthropicCodec;
use switchyard_core::reasoning::{
    Depth, Effort, Fitted, ModelThinking, ReasoningConfig, Summary, ThinkingSupport,
};
use switchyard_core::{
    Codec, CodecError, ModelInfo, Protocol, RequestMeta, RequestPath, UpstreamCtx,
};

fn write(mut body: Value, depth: Fitted, ctx: &UpstreamCtx<'_>) -> Value {
    AnthropicCodec.write_reasoning(&mut body, depth, ctx);
    body
}

fn unknown() -> UpstreamCtx<'static> {
    UpstreamCtx::default()
}

fn budget_only() -> ThinkingSupport {
    ThinkingSupport {
        min: 1024,
        max: 128_000,
        zero_allowed: true,
        dynamic_allowed: false,
        levels: vec![],
    }
}

fn level_only() -> ThinkingSupport {
    ThinkingSupport {
        dynamic_allowed: true,
        ..ThinkingSupport::levels(&[Effort::Low, Effort::Medium, Effort::High])
    }
}

fn hybrid() -> ThinkingSupport {
    ThinkingSupport {
        min: 1024,
        max: 64_000,
        zero_allowed: true,
        dynamic_allowed: true,
        levels: vec![Effort::Low, Effort::Medium, Effort::High, Effort::Max],
    }
}

fn supported(caps: &ThinkingSupport, max_output_tokens: Option<u64>) -> UpstreamCtx<'_> {
    UpstreamCtx {
        thinking: ModelThinking::Supported(caps),
        max_output_tokens,
        ..UpstreamCtx::default()
    }
}

// ---------------------------------------------------------------------------
// protocol / request_meta / set_request_model
// ---------------------------------------------------------------------------

#[test]
fn protocol_is_anthropic() {
    assert_eq!(AnthropicCodec.protocol(), Protocol::Anthropic);
}

#[test]
fn request_meta_reads_model_and_stream() {
    let meta = |body: Value| AnthropicCodec.request_meta(&body, &RequestPath::default());
    assert_eq!(
        meta(json!({"model": "claude-sonnet-4-5(high)", "stream": true, "messages": []})),
        Ok(RequestMeta {
            model: "claude-sonnet-4-5(high)".into(),
            stream: true
        })
    );
    // Only the JSON literal `true` streams.
    for stream in [
        json!(false),
        json!("true"),
        json!(1),
        Value::Null,
        json!({}),
    ] {
        assert_eq!(
            meta(json!({"model": "m", "stream": stream})).map(|m| m.stream),
            Ok(false)
        );
    }
    assert_eq!(meta(json!({"model": "m"})).map(|m| m.stream), Ok(false));
}

#[test]
fn request_meta_errors() {
    let path = RequestPath::default();
    assert_eq!(
        AnthropicCodec.request_meta(&json!({"messages": []}), &path),
        Err(CodecError::invalid_param("model", "`model` is required"))
    );
    assert!(
        AnthropicCodec
            .request_meta(&json!({"model": ""}), &path)
            .is_err()
    );
    assert!(
        AnthropicCodec
            .request_meta(&json!({"model": 7}), &path)
            .is_err()
    );
    assert!(AnthropicCodec.request_meta(&json!([1, 2]), &path).is_err());
}

#[test]
fn set_request_model_overwrites_only_the_model() {
    let mut body = json!({"model": "alias(high)", "max_tokens": 5, "messages": []});
    AnthropicCodec.set_request_model(&mut body, "claude-sonnet-4-5-20250929");
    assert_eq!(
        body,
        json!({"model": "claude-sonnet-4-5-20250929", "max_tokens": 5, "messages": []})
    );
    // Key order of a forwarded body is preserved.
    assert_eq!(
        body.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["model", "max_tokens", "messages"]
    );
    let mut not_an_object = json!("x");
    AnthropicCodec.set_request_model(&mut not_an_object, "m");
    assert_eq!(not_an_object, json!("x"));
}

// ---------------------------------------------------------------------------
// rewrite_response_model
// ---------------------------------------------------------------------------

#[test]
fn rewrite_response_model_on_a_full_response() {
    let mut body = json!({
        "id": "msg_1", "type": "message", "role": "assistant",
        "model": "claude-sonnet-4-5-20250929",
        "content": [{"type": "text", "text": "the model field in text stays: claude-sonnet-4-5-20250929"}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    AnthropicCodec.rewrite_response_model(&mut body, "sonnet");
    assert_eq!(body["model"], json!("sonnet"));
    assert_eq!(
        body["content"][0]["text"],
        json!("the model field in text stays: claude-sonnet-4-5-20250929")
    );
}

#[test]
fn rewrite_response_model_on_stream_event_payloads() {
    let mut message_start = json!({"type": "message_start", "message": {
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5-20250929",
        "content": [], "stop_reason": null, "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}}});
    AnthropicCodec.rewrite_response_model(&mut message_start, "sonnet");
    assert_eq!(message_start["message"]["model"], json!("sonnet"));
    assert!(message_start.get("model").is_none());

    // Events without a model are left exactly as they are.
    for payload in [
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "model"}}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "tool_use", "id": "t", "name": "set_model", "input": {}}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
        json!({"type": "message_stop"}),
        json!({"type": "ping"}),
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
        json!([1, 2, 3]),
    ] {
        let mut rewritten = payload.clone();
        AnthropicCodec.rewrite_response_model(&mut rewritten, "sonnet");
        assert_eq!(rewritten, payload);
    }
}

// ---------------------------------------------------------------------------
// read_reasoning
// ---------------------------------------------------------------------------

#[test]
fn read_reasoning_from_raw_bodies() {
    let read = |body: Value| AnthropicCodec.read_reasoning(&body);
    assert_eq!(read(json!({"model": "m"})), ReasoningConfig::default());
    assert!(read(json!("not an object")).is_empty());
    assert_eq!(
        read(json!({"thinking": {"type": "enabled", "budget_tokens": 16000}})),
        ReasoningConfig::with_depth(Depth::Budget(16000))
    );
    assert_eq!(
        read(
            json!({"thinking": {"type": "adaptive", "display": "summarized"},
                    "output_config": {"effort": "xhigh"}})
        ),
        ReasoningConfig {
            depth: Some(Depth::Level(Effort::Xhigh)),
            summary: Some(Summary::Auto),
        }
    );
    assert_eq!(
        read(json!({"thinking": {"type": "disabled"}})),
        ReasoningConfig::with_depth(Depth::Off)
    );
    assert_eq!(
        read(json!({"thinking": {"type": "adaptive"}})),
        ReasoningConfig::with_depth(Depth::Auto)
    );
    // An effort this gateway does not know still means "think".
    assert_eq!(
        read(json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "ludicrous"}})),
        ReasoningConfig::with_depth(Depth::Auto)
    );
}

// ---------------------------------------------------------------------------
// write_reasoning
// ---------------------------------------------------------------------------

#[test]
fn write_reasoning_off() {
    let body = json!({
        "model": "m", "max_tokens": 1000,
        "thinking": {"type": "enabled", "budget_tokens": 5000, "display": "summarized"},
        "output_config": {"effort": "high", "format": {"type": "json_schema", "schema": {}}}
    });
    let expected = json!({
        "model": "m", "max_tokens": 1000,
        "thinking": {"type": "disabled"},
        "output_config": {"format": {"type": "json_schema", "schema": {}}}
    });
    let caps = [budget_only(), level_only(), hybrid()];
    assert_eq!(
        write(body.clone(), Fitted::Use(Depth::Off), &unknown()),
        expected
    );
    for caps in &caps {
        assert_eq!(
            write(
                body.clone(),
                Fitted::Use(Depth::Off),
                &supported(caps, Some(64_000))
            ),
            expected
        );
    }
    // A body that said nothing gets an explicit "disabled".
    assert_eq!(
        write(json!({"model": "m"}), Fitted::Use(Depth::Off), &unknown()),
        json!({"model": "m", "thinking": {"type": "disabled"}})
    );
}

#[test]
fn write_reasoning_level() {
    let body = json!({
        "max_tokens": 16000,
        "thinking": {"type": "enabled", "budget_tokens": 5000, "display": "summarized"}
    });
    // Unknown model: the modern form, unclamped.
    assert_eq!(
        write(
            body.clone(),
            Fitted::Use(Depth::Level(Effort::Xhigh)),
            &unknown()
        ),
        json!({"max_tokens": 16000,
               "thinking": {"type": "adaptive", "display": "summarized"},
               "output_config": {"effort": "xhigh"}})
    );
    // Level model: clamped to what it lists ("max" → its top level).
    let caps = level_only();
    assert_eq!(
        write(
            body.clone(),
            Fitted::Use(Depth::Level(Effort::Max)),
            &supported(&caps, None)
        ),
        json!({"max_tokens": 16000,
               "thinking": {"type": "adaptive", "display": "summarized"},
               "output_config": {"effort": "high"}})
    );
    // Hybrid model: levels win for a level.
    let caps = hybrid();
    assert_eq!(
        write(
            body.clone(),
            Fitted::Use(Depth::Level(Effort::Xhigh)),
            &supported(&caps, None)
        ),
        json!({"max_tokens": 16000,
               "thinking": {"type": "adaptive", "display": "summarized"},
               "output_config": {"effort": "max"}})
    );
    // Budget-only model: the level's table budget.
    let caps = budget_only();
    assert_eq!(
        write(
            body.clone(),
            Fitted::Use(Depth::Level(Effort::Medium)),
            &supported(&caps, None)
        ),
        json!({"max_tokens": 16000,
               "thinking": {"type": "enabled", "budget_tokens": 8192, "display": "summarized"}})
    );
    // "minimal" does not exist on the wire.
    assert_eq!(
        write(
            json!({}),
            Fitted::Use(Depth::Level(Effort::Minimal)),
            &unknown()
        ),
        json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "low"}})
    );
}

#[test]
fn write_reasoning_budget() {
    let body = json!({
        "max_tokens": 16000,
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "high"}
    });
    let manual =
        json!({"max_tokens": 16000, "thinking": {"type": "enabled", "budget_tokens": 4000}});
    assert_eq!(
        write(body.clone(), Fitted::Use(Depth::Budget(4000)), &unknown()),
        manual
    );
    let caps = budget_only();
    assert_eq!(
        write(
            body.clone(),
            Fitted::Use(Depth::Budget(4000)),
            &supported(&caps, None)
        ),
        manual
    );
    let caps = hybrid();
    assert_eq!(
        write(
            body.clone(),
            Fitted::Use(Depth::Budget(4000)),
            &supported(&caps, None)
        ),
        manual
    );
    // A model that only takes levels gets the nearest effort instead.
    let caps = level_only();
    assert_eq!(
        write(
            body.clone(),
            Fitted::Use(Depth::Budget(4000)),
            &supported(&caps, None)
        ),
        json!({"max_tokens": 16000, "thinking": {"type": "adaptive"},
               "output_config": {"effort": "medium"}})
    );
    assert_eq!(
        write(
            body.clone(),
            Fitted::Use(Depth::Budget(100_000)),
            &supported(&caps, None)
        ),
        json!({"max_tokens": 16000, "thinking": {"type": "adaptive"},
               "output_config": {"effort": "high"}})
    );
}

#[test]
fn write_reasoning_budget_stays_below_max_tokens() {
    let caps = budget_only();
    let ctx = supported(&caps, Some(64_000));
    // Shrunk under the body's limit.
    assert_eq!(
        write(
            json!({"max_tokens": 4096}),
            Fitted::Use(Depth::Budget(8192)),
            &ctx
        ),
        json!({"max_tokens": 4096, "thinking": {"type": "enabled", "budget_tokens": 4095}})
    );
    // The limit cannot hold even the minimum budget: raised to the model's.
    assert_eq!(
        write(
            json!({"max_tokens": 512}),
            Fitted::Use(Depth::Budget(8192)),
            &ctx
        ),
        json!({"max_tokens": 64000, "thinking": {"type": "enabled", "budget_tokens": 8192}})
    );
    // No limit in the body: the model's is written.
    assert_eq!(
        write(json!({}), Fitted::Use(Depth::Budget(8192)), &ctx),
        json!({"max_tokens": 64000, "thinking": {"type": "enabled", "budget_tokens": 8192}})
    );
    // Under the API minimum: raised to it.
    assert_eq!(
        write(
            json!({"max_tokens": 4096}),
            Fitted::Use(Depth::Budget(10)),
            &ctx
        ),
        json!({"max_tokens": 4096, "thinking": {"type": "enabled", "budget_tokens": 1024}})
    );
    // Nothing known about the model and no limit: left for the upstream.
    assert_eq!(
        write(json!({}), Fitted::Use(Depth::Budget(8192)), &unknown()),
        json!({"thinking": {"type": "enabled", "budget_tokens": 8192}})
    );
}

/// `budget_tokens >= max_tokens` is a 400. When the limit cannot hold even
/// the smallest budget and there is no larger limit to move to, the request
/// goes out without extended thinking.
#[test]
fn write_reasoning_budget_is_left_out_when_no_valid_budget_fits() {
    // Nothing known about the model: the client's limit is all there is.
    for max_tokens in [1_u64, 256, 1000, 1024] {
        assert_eq!(
            write(
                json!({"max_tokens": max_tokens, "thinking": {"type": "adaptive", "display": "summarized"},
                       "output_config": {"effort": "high", "format": {"type": "json_schema", "schema": {}}}}),
                Fitted::Use(Depth::Budget(8192)),
                &unknown()
            ),
            json!({"max_tokens": max_tokens,
                   "output_config": {"format": {"type": "json_schema", "schema": {}}}}),
            "max_tokens {max_tokens}"
        );
    }
    // One token more and the minimum budget fits.
    assert_eq!(
        write(
            json!({"max_tokens": 1025}),
            Fitted::Use(Depth::Budget(8192)),
            &unknown()
        ),
        json!({"max_tokens": 1025, "thinking": {"type": "enabled", "budget_tokens": 1024}})
    );
    // A known model whose own limit is no larger than the body's.
    let caps = budget_only();
    assert_eq!(
        write(
            json!({"max_tokens": 1024}),
            Fitted::Use(Depth::Budget(8192)),
            &supported(&caps, Some(1024))
        ),
        json!({"max_tokens": 1024})
    );
    // A cache pre-warm (`max_tokens: 0`) stays one: the limit is not raised
    // to make room for thinking.
    assert_eq!(
        write(
            json!({"max_tokens": 0}),
            Fitted::Use(Depth::Budget(8192)),
            &supported(&caps, Some(64_000))
        ),
        json!({"max_tokens": 0})
    );
    // A level on a budget-only model takes the same path.
    assert_eq!(
        write(
            json!({"max_tokens": 512}),
            Fitted::Use(Depth::Level(Effort::High)),
            &supported(&caps, None)
        ),
        json!({"max_tokens": 512})
    );
}

/// Notes 15 section 5.6: with `enabled`, "the final assistant turn must
/// begin with a thinking block; adaptive mode drops that rule". A depth
/// forced by a model suffix must not turn a valid body into a refused one.
#[test]
fn write_reasoning_manual_thinking_needs_a_turn_that_opens_with_thinking() {
    let caps = budget_only();
    let ctx = supported(&caps, Some(64_000));
    let body = |messages: Value| json!({"model": "claude-sonnet-4-5", "max_tokens": 16000, "messages": messages});
    let unsigned_loop = json!([
        {"role": "user", "content": "What is the answer?"},
        {"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}}
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "42"}
        ]}
    ]);
    // A tool loop in progress without thinking at its start: no manual
    // thinking for this request.
    assert_eq!(
        write(
            body(unsigned_loop.clone()),
            Fitted::Use(Depth::Budget(8192)),
            &ctx
        ),
        body(unsigned_loop.clone())
    );
    // The rule is Anthropic's: another vendor's model served over this
    // protocol gets what was asked.
    assert_eq!(
        write(
            json!({"model": "glm-4.6", "max_tokens": 16000, "messages": unsigned_loop.clone()}),
            Fitted::Use(Depth::Budget(8192)),
            &ctx
        )["thinking"],
        json!({"type": "enabled", "budget_tokens": 8192})
    );
    // Adaptive thinking is not bound by the rule.
    let levels = level_only();
    assert_eq!(
        write(
            body(unsigned_loop.clone()),
            Fitted::Use(Depth::Level(Effort::High)),
            &supported(&levels, Some(64_000))
        )["thinking"],
        json!({"type": "adaptive"})
    );

    // The turn opens with thinking. Its later steps need none of their own
    // (without interleaved thinking the model thinks once per turn).
    let signed_loop = json!([
        {"role": "user", "content": "What is the answer?"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "Look it up.", "signature": "EqQBCkYIBBgCKkD"},
            {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}}
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "42"}
        ]},
        {"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_2", "name": "lookup", "input": {}}
        ]},
        {"role": "system", "content": "A reminder between the steps."},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_2", "content": "43"}
        ]}
    ]);
    assert_eq!(
        write(body(signed_loop), Fitted::Use(Depth::Budget(8192)), &ctx)["thinking"],
        json!({"type": "enabled", "budget_tokens": 8192})
    );

    // A new turn: earlier turns without thinking do not matter.
    let mut finished = unsigned_loop.clone();
    finished.as_array_mut().unwrap().extend([
        json!({"role": "assistant", "content": "It is 42."}),
        json!({"role": "user", "content": "And twice that?"}),
    ]);
    assert_eq!(
        write(body(finished), Fitted::Use(Depth::Budget(8192)), &ctx)["thinking"],
        json!({"type": "enabled", "budget_tokens": 8192})
    );

    // No conversation at all (a body being built, a malformed one): the
    // depth is written as asked.
    for messages in [json!([]), json!("nonsense"), json!([1, "x", null])] {
        assert_eq!(
            write(body(messages), Fitted::Use(Depth::Budget(8192)), &ctx)["thinking"],
            json!({"type": "enabled", "budget_tokens": 8192})
        );
    }
}

#[test]
fn write_reasoning_auto() {
    let body = json!({
        "max_tokens": 16000,
        "thinking": {"type": "enabled", "budget_tokens": 5000, "display": "omitted"},
        "output_config": {"effort": "low"}
    });
    let adaptive =
        json!({"max_tokens": 16000, "thinking": {"type": "adaptive", "display": "omitted"}});
    assert_eq!(
        write(body.clone(), Fitted::Use(Depth::Auto), &unknown()),
        adaptive
    );
    let caps = level_only();
    assert_eq!(
        write(
            body.clone(),
            Fitted::Use(Depth::Auto),
            &supported(&caps, None)
        ),
        adaptive
    );
    let caps = hybrid();
    assert_eq!(
        write(
            body.clone(),
            Fitted::Use(Depth::Auto),
            &supported(&caps, None)
        ),
        adaptive
    );
    // A manual-only model cannot be told "you decide": say nothing.
    let caps = budget_only();
    assert_eq!(
        write(
            body.clone(),
            Fitted::Use(Depth::Auto),
            &supported(&caps, None)
        ),
        json!({"max_tokens": 16000})
    );
}

#[test]
fn write_reasoning_strip_and_unsupported_models() {
    let body = json!({
        "model": "m", "max_tokens": 1000, "temperature": 0.3,
        "thinking": {"type": "enabled", "budget_tokens": 5000},
        "output_config": {"effort": "high"},
        "messages": []
    });
    let stripped = json!({"model": "m", "max_tokens": 1000, "temperature": 0.3, "messages": []});
    assert_eq!(write(body.clone(), Fitted::Strip, &unknown()), stripped);
    let caps = hybrid();
    assert_eq!(
        write(body.clone(), Fitted::Strip, &supported(&caps, None)),
        stripped
    );
    // A model known not to think: whatever is asked, the fields go.
    let unsupported = UpstreamCtx {
        thinking: ModelThinking::Unsupported,
        ..UpstreamCtx::default()
    };
    for depth in [
        Depth::Off,
        Depth::Auto,
        Depth::Level(Effort::High),
        Depth::Budget(4000),
    ] {
        assert_eq!(
            write(body.clone(), Fitted::Use(depth), &unsupported),
            stripped,
            "{depth:?}"
        );
    }
    // `format` is not a reasoning field.
    assert_eq!(
        write(
            json!({"output_config": {"effort": "high", "format": {"type": "json_schema", "schema": {}}}}),
            Fitted::Strip,
            &unknown()
        ),
        json!({"output_config": {"format": {"type": "json_schema", "schema": {}}}})
    );
}

#[test]
fn write_reasoning_keeps_the_body_valid() {
    // Forced tool use and thinking are mutually exclusive.
    let body = json!({
        "max_tokens": 16000, "tools": [{"name": "f", "input_schema": {"type": "object"}}],
        "tool_choice": {"type": "any"}
    });
    assert_eq!(
        write(
            body.clone(),
            Fitted::Use(Depth::Level(Effort::High)),
            &unknown()
        ),
        body
    );
    let mut named = body.clone();
    named["tool_choice"] = json!({"type": "tool", "name": "f"});
    assert_eq!(
        write(named.clone(), Fitted::Use(Depth::Budget(2000)), &unknown()),
        named
    );
    let mut auto = body.clone();
    auto["tool_choice"] = json!({"type": "auto"});
    assert_eq!(
        write(auto, Fitted::Use(Depth::Level(Effort::High)), &unknown())["thinking"],
        json!({"type": "adaptive"})
    );

    // Sampling values the API refuses next to thinking are removed; the ones
    // it accepts stay (this is the client's own body).
    let sampled = |temperature: f64, top_p: f64| {
        write(
            json!({"max_tokens": 16000, "temperature": temperature, "top_p": top_p, "top_k": 40}),
            Fitted::Use(Depth::Budget(2000)),
            &unknown(),
        )
    };
    let body = sampled(0.7, 0.5);
    assert!(
        body.get("temperature").is_none()
            && body.get("top_p").is_none()
            && body.get("top_k").is_none()
    );
    let body = sampled(1.0, 0.99);
    assert_eq!(
        (&body["temperature"], &body["top_p"]),
        (&json!(1.0), &json!(0.99))
    );
    assert!(body.get("top_k").is_none());
    // Thinking off: sampling is none of this function's business.
    let body = write(
        json!({"temperature": 0.7, "top_p": 0.5, "top_k": 40}),
        Fitted::Use(Depth::Off),
        &unknown(),
    );
    assert_eq!(
        body,
        json!({"temperature": 0.7, "top_p": 0.5, "top_k": 40, "thinking": {"type": "disabled"}})
    );
}

#[test]
fn write_reasoning_tolerates_malformed_bodies() {
    assert_eq!(
        write(json!("text"), Fitted::Use(Depth::Auto), &unknown()),
        json!("text")
    );
    assert_eq!(
        write(
            json!({"thinking": "yes please", "output_config": 5}),
            Fitted::Use(Depth::Level(Effort::Low)),
            &unknown()
        ),
        json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "low"}})
    );
}

#[test]
fn write_then_read_reasoning_is_consistent() {
    for depth in [
        Depth::Off,
        Depth::Auto,
        Depth::Level(Effort::Low),
        Depth::Level(Effort::High),
        Depth::Level(Effort::Max),
        Depth::Budget(2048),
    ] {
        let body = write(json!({"max_tokens": 32000}), Fitted::Use(depth), &unknown());
        assert_eq!(
            AnthropicCodec.read_reasoning(&body).depth,
            Some(depth),
            "{depth:?}"
        );
    }
    let body = write(
        json!({"thinking": {"type": "enabled", "budget_tokens": 9000}}),
        Fitted::Strip,
        &unknown(),
    );
    assert!(AnthropicCodec.read_reasoning(&body).is_empty());
}

// ---------------------------------------------------------------------------
// prepare_passthrough
// ---------------------------------------------------------------------------

#[test]
fn prepare_passthrough_changes_nothing_in_a_complete_body() {
    let body = json!({
        "model": "claude-opus-4-5",
        "max_tokens": 1000,
        "temperature": 0.2, "top_p": 0.9, "top_k": 5,
        "system": "You are helpful.",
        "thinking": {"type": "enabled", "budget_tokens": 5000},
        "messages": [
            {"role": "user", "content": "What is the weather in Paris?"},
            {"role": "system", "content": "mid-conversation system"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": "EqQBCkYIBBgCKkD"},
                {"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix"},
                {"type": "text", "text": "Let me check.",
                 "citations": [{"type": "web_search_result_location", "url": "https://e.com",
                                "title": "E", "encrypted_index": "Eo8BCioIAhgBIiQ", "cited_text": "x"}]},
                {"type": "server_tool_use", "id": "srvtoolu_01", "name": "web_search",
                 "input": {"query": "weather paris"}},
                {"type": "tool_use", "id": "toolu_01A", "name": "f", "input": {}},
                {"type": "tool_use", "id": "toolu_01B", "name": "f", "input": {"x": " "}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01A", "content": " "},
                {"type": "tool_result", "tool_use_id": "toolu_01B",
                 "content": [{"type": "text", "text": "ok"}], "is_error": false},
                {"type": "text", "text": "  "}
            ]},
            // Without interleaved thinking the later steps of a tool loop
            // carry no thinking block of their own.
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_01C", "name": "f", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01C", "content": "done"}
            ]}
        ],
        "tools": [
            {"type": "web_search_20250305", "name": "web_search", "allowed_domains": ["example.com"]},
            {"name": "f", "input_schema": {"type": "object", "properties": {"allowed_domains": {"type": "array"}}}}
        ],
        "tool_choice": {"type": "any"},
        "betas": ["x"],
        "stream": true,
        "unknown_future_field": {"a": [1, 2, 3]}
    });
    let mut forwarded = body.clone();
    let ctx = UpstreamCtx {
        max_output_tokens: Some(64_000),
        ..UpstreamCtx::default()
    };
    AnthropicCodec.prepare_passthrough(&mut forwarded, true, &ctx);
    assert_eq!(forwarded, body);
    assert_eq!(
        forwarded.as_object().unwrap().keys().collect::<Vec<_>>(),
        body.as_object().unwrap().keys().collect::<Vec<_>>()
    );
}

#[test]
fn prepare_passthrough_fills_in_the_mandatory_max_tokens() {
    let known = UpstreamCtx {
        max_output_tokens: Some(64_000),
        ..UpstreamCtx::default()
    };
    let mut body = json!({"model": "m", "messages": []});
    AnthropicCodec.prepare_passthrough(&mut body, false, &known);
    assert_eq!(
        body,
        json!({"model": "m", "messages": [], "max_tokens": 64000})
    );
    let mut body = json!({"model": "m", "messages": [], "max_tokens": null});
    AnthropicCodec.prepare_passthrough(&mut body, false, &known);
    assert_eq!(body["max_tokens"], json!(64000));
    // A legal `max_tokens: 0` (cache pre-warm) is the client's choice.
    let mut body = json!({"model": "m", "messages": [], "max_tokens": 0});
    AnthropicCodec.prepare_passthrough(&mut body, false, &known);
    assert_eq!(body["max_tokens"], json!(0));
    // Nothing known about the model: nothing invented.
    let mut body = json!({"model": "m", "messages": []});
    AnthropicCodec.prepare_passthrough(&mut body, false, &UpstreamCtx::default());
    assert_eq!(body, json!({"model": "m", "messages": []}));
}

#[test]
fn prepare_passthrough_makes_stream_agree_with_the_transport() {
    let ctx = UpstreamCtx::default();
    let mut body = json!({"model": "m", "max_tokens": 1});
    AnthropicCodec.prepare_passthrough(&mut body, true, &ctx);
    assert_eq!(body["stream"], json!(true));
    // The gateway treated a non-literal value as "not streaming"; the
    // upstream must see the same thing.
    let mut body = json!({"model": "m", "max_tokens": 1, "stream": "yes"});
    AnthropicCodec.prepare_passthrough(&mut body, false, &ctx);
    assert_eq!(body["stream"], json!(false));
    let mut body = json!({"model": "m", "max_tokens": 1});
    AnthropicCodec.prepare_passthrough(&mut body, false, &ctx);
    assert!(body.get("stream").is_none());
    // Not an object: untouched.
    let mut body = json!([1]);
    AnthropicCodec.prepare_passthrough(&mut body, true, &ctx);
    assert_eq!(body, json!([1]));
}

#[test]
fn prepare_passthrough_removes_empty_domain_filters_of_web_tools() {
    let mut body = json!({
        "model": "m", "max_tokens": 1,
        "tools": [
            {"type": "web_search_20250305", "name": "web_search", "allowed_domains": [], "blocked_domains": []},
            {"type": "web_fetch_20250910", "name": "web_fetch", "allowed_domains": [], "max_uses": 2},
            {"type": "web_search_20260209", "name": "web_search", "blocked_domains": ["bad.example"]},
            {"name": "custom", "input_schema": {"type": "object"}, "allowed_domains": []}
        ]
    });
    AnthropicCodec.prepare_passthrough(&mut body, false, &UpstreamCtx::default());
    assert_eq!(
        body["tools"],
        json!([
            {"type": "web_search_20250305", "name": "web_search"},
            {"type": "web_fetch_20250910", "name": "web_fetch", "max_uses": 2},
            {"type": "web_search_20260209", "name": "web_search", "blocked_domains": ["bad.example"]},
            {"name": "custom", "input_schema": {"type": "object"}, "allowed_domains": []}
        ])
    );
}

fn passthrough(mut body: Value) -> Value {
    AnthropicCodec.prepare_passthrough(&mut body, false, &UpstreamCtx::default());
    // Whatever is repaired is repaired once: the same body goes upstream on
    // every later request of the conversation (prompt caching and thinking
    // signatures depend on an unchanging prefix).
    let mut again = body.clone();
    AnthropicCodec.prepare_passthrough(&mut again, false, &UpstreamCtx::default());
    assert_eq!(again, body, "prepare_passthrough must be idempotent");
    body
}

/// What a Messages client holds after another vendor served the first turn
/// of a conversation: reasoning without a signature (rendered by this codec
/// with `"signature": ""`), whitespace-only text, a citation the API did not
/// issue.
#[test]
fn prepare_passthrough_removes_assistant_blocks_anthropic_did_not_issue() {
    let body = passthrough(json!({
        "model": "claude-sonnet-4-5", "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "The user greets me.", "signature": ""},
                {"type": "thinking", "thinking": "No signature key at all."},
                {"type": "redacted_thinking", "data": ""},
                {"type": "text", "text": "\n\n"},
                {"type": "text", "text": "Hello!", "citations": [
                    {"type": "web_search_result_location", "url": "https://e.com", "title": "E",
                     "encrypted_index": "", "cited_text": ""}
                ]},
                {"type": "thinking", "thinking": "", "signature": "EqQBCkYIBBgCKkD"}
            ]},
            {"role": "user", "content": "and now?"}
        ]
    }));
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Hello!"},
                {"type": "thinking", "thinking": "", "signature": "EqQBCkYIBBgCKkD"}
            ]},
            {"role": "user", "content": "and now?"}
        ])
    );
}

#[test]
fn prepare_passthrough_removes_a_message_left_without_content() {
    let body = passthrough(json!({
        "model": "claude-opus-4-5", "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Only unsigned reasoning.", "signature": ""}
            ]},
            {"role": "user", "content": "still there?"},
            // An empty content list the client wrote itself is its own.
            {"role": "assistant", "content": []},
            {"role": "user", "content": "hello?"}
        ]
    }));
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "hi"},
            {"role": "user", "content": "still there?"},
            {"role": "assistant", "content": []},
            {"role": "user", "content": "hello?"}
        ])
    );
}

/// Thinking blocks of other models served over this protocol are theirs to
/// judge: such endpoints issue empty or free-form signatures and may want
/// the block back.
#[test]
fn prepare_passthrough_leaves_the_history_of_other_models_alone() {
    let original = json!({
        "model": "glm-4.6", "max_tokens": 16000,
        // Manual thinking for a turn that does not open with a thinking
        // block: Anthropic's rule, not this model's.
        "thinking": {"type": "enabled", "budget_tokens": 4096},
        "messages": [
            {"role": "user", "content": "earlier"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Let me look.", "signature": ""},
                {"type": "text", "text": " "},
                {"type": "tool_use", "id": "call:1", "name": "f", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call:1", "content": "41"}
            ]},
            {"role": "assistant", "content": [{"type": "text", "text": "It is 41."}]},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call:1", "name": "f", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call:1", "content": "42"}
            ]}
        ]
    });
    assert_eq!(passthrough(original.clone()), original);
    // The very same body addressed to a Claude model is repaired.
    let mut for_claude = original.clone();
    for_claude["model"] = json!("claude-sonnet-4-5");
    let repaired = passthrough(for_claude);
    assert!(repaired.get("thinking").is_none());
    assert_eq!(
        repaired["messages"][1]["content"],
        json!([{"type": "tool_use", "id": "call_1", "name": "f", "input": {}}])
    );
    assert_ne!(repaired["messages"][5]["content"][0]["id"], json!("call_1"));
}

/// Call ids issued by another vendor reach a Messages client verbatim; the
/// API only accepts `[a-zA-Z0-9_-]+` and wants every `tool_use` id unique.
#[test]
fn prepare_passthrough_makes_foreign_tool_ids_acceptable() {
    let body = passthrough(json!({
        "model": "claude-sonnet-4-5", "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "Weather in Paris?"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "functions.get_weather:0", "name": "get_weather",
                 "input": {"city": "Paris"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "functions.get_weather:0", "content": "sunny"}
            ]},
            {"role": "assistant", "content": [{"type": "text", "text": "Sunny."}]},
            {"role": "user", "content": "And in Rome? And is toolu_01A fine?"},
            {"role": "assistant", "content": [
                // The same vendor id again, next to a native one.
                {"type": "tool_use", "id": "functions.get_weather:0", "name": "get_weather",
                 "input": {"city": "Rome"}},
                {"type": "tool_use", "id": "toolu_01A", "name": "get_weather",
                 "input": {"city": "Oslo"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01A", "content": "snow"},
                {"type": "tool_result", "tool_use_id": "functions.get_weather:0", "content": "rainy"}
            ]}
        ]
    }));
    let messages = body["messages"].as_array().unwrap();
    let first = messages[1]["content"][0]["id"].as_str().unwrap();
    let second = messages[5]["content"][0]["id"].as_str().unwrap();
    assert_eq!(first, "functions_get_weather_0");
    assert_ne!(first, second);
    assert!(second.starts_with("functions_get_weather_0_"));
    assert!(
        second
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    );
    assert_eq!(messages[5]["content"][1]["id"], json!("toolu_01A"));
    assert_eq!(messages[2]["content"][0]["tool_use_id"], json!(first));
    assert_eq!(messages[2]["content"][0]["content"], json!("sunny"));
    assert_eq!(
        messages[6]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "toolu_01A", "content": "snow"},
            {"type": "tool_result", "tool_use_id": second, "content": "rainy"}
        ])
    );
    // Everything else is as the client sent it.
    assert_eq!(messages[5]["content"][0]["input"], json!({"city": "Rome"}));
    assert_eq!(
        messages[4],
        json!({"role": "user", "content": "And in Rome? And is toolu_01A fine?"})
    );
}

/// Notes 15 section 5.6: with `enabled`, "the final assistant turn must
/// begin with a thinking block; adaptive mode drops that rule". A tool loop
/// begun by another vendor cannot satisfy it, and neither can one whose
/// unsigned thinking has just been removed.
#[test]
fn prepare_passthrough_drops_manual_thinking_for_a_turn_that_cannot_have_it() {
    let tool_loop = |first_block: Value, thinking: Value| {
        json!({
            "model": "claude-sonnet-4-5", "max_tokens": 16000, "thinking": thinking,
            "output_config": {"effort": "high"},
            "messages": [
                {"role": "user", "content": "What is the answer?"},
                {"role": "assistant", "content": [
                    first_block,
                    {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "42"},
                    {"type": "text", "text": "<system-reminder>be brief</system-reminder>"}
                ]}
            ]
        })
    };
    let manual = json!({"type": "enabled", "budget_tokens": 8192});

    // The turn opens with text: served by a vendor that returned no thinking.
    let body = passthrough(tool_loop(
        json!({"type": "text", "text": "Looking it up."}),
        manual.clone(),
    ));
    assert!(body.get("thinking").is_none());
    assert_eq!(body["output_config"], json!({"effort": "high"}));
    assert_eq!(body["messages"][1]["content"][0]["type"], json!("text"));

    // The turn opened with unsigned thinking, which is removed first.
    let body = passthrough(tool_loop(
        json!({"type": "thinking", "thinking": "Hm.", "signature": ""}),
        manual.clone(),
    ));
    assert!(body.get("thinking").is_none());
    assert_eq!(
        body["messages"][1]["content"],
        json!([{"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}}])
    );

    // Signed thinking at the start of the turn: nothing changes.
    for opening in [
        json!({"type": "thinking", "thinking": "Hm.", "signature": "EqQBCkYIBBgCKkD"}),
        json!({"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix"}),
    ] {
        let original = tool_loop(opening, manual.clone());
        assert_eq!(passthrough(original.clone()), original);
    }

    // Adaptive thinking has no such rule.
    let original = tool_loop(
        json!({"type": "text", "text": "Looking it up."}),
        json!({"type": "adaptive"}),
    );
    assert_eq!(passthrough(original.clone()), original);

    // A conversation that ends with an ordinary user message starts a new
    // turn; what earlier turns look like does not matter.
    let mut original = tool_loop(
        json!({"type": "text", "text": "Looking it up."}),
        manual.clone(),
    );
    original["messages"].as_array_mut().unwrap().extend([
        json!({"role": "assistant", "content": [{"type": "text", "text": "It is 42."}]}),
        json!({"role": "user", "content": "Thanks. Another question."}),
    ]);
    assert_eq!(passthrough(original.clone()), original);

    // A prefill is a turn in progress as well.
    let body = passthrough(json!({
        "model": "claude-sonnet-4-5", "max_tokens": 16000, "thinking": manual,
        "messages": [
            {"role": "user", "content": "List three colours."},
            {"role": "assistant", "content": "1."}
        ]
    }));
    assert!(body.get("thinking").is_none());
}

// ---------------------------------------------------------------------------
// Model listings
// ---------------------------------------------------------------------------

#[test]
fn encode_models_in_the_vendor_shape() {
    let models = vec![
        ModelInfo {
            id: "claude-sonnet-4-5".into(),
            display_name: Some("Claude Sonnet 4.5".into()),
            description: Some("not part of the Anthropic shape".into()),
            owned_by: Some("anthropic".into()),
            created: Some(1_759_104_000),
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
            thinking: Some(budget_only()),
            known: true,
        },
        ModelInfo::bare("team/my-local-model"),
    ];
    assert_eq!(
        AnthropicCodec.encode_models(&models),
        json!({
            "data": [
                {"type": "model", "id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5",
                 "created_at": "2025-09-29T00:00:00Z", "max_input_tokens": 200000, "max_tokens": 64000},
                {"type": "model", "id": "team/my-local-model", "display_name": "team/my-local-model",
                 "created_at": "1970-01-01T00:00:00Z", "max_input_tokens": null, "max_tokens": null}
            ],
            "has_more": false,
            "first_id": "claude-sonnet-4-5",
            "last_id": "team/my-local-model"
        })
    );
    assert_eq!(
        AnthropicCodec.encode_models(&[]),
        json!({"data": [], "has_more": false, "first_id": null, "last_id": null})
    );
}

#[test]
fn encode_model_is_a_single_object() {
    let model = ModelInfo {
        id: "gemini-2.5-pro".into(),
        display_name: Some("Gemini 2.5 Pro".into()),
        created: Some(1_750_118_400),
        context_window: Some(1_048_576),
        max_output_tokens: Some(65_536),
        ..ModelInfo::default()
    };
    assert_eq!(
        AnthropicCodec.encode_model(&model),
        json!({"type": "model", "id": "gemini-2.5-pro", "display_name": "Gemini 2.5 Pro",
               "created_at": "2025-06-17T00:00:00Z", "max_input_tokens": 1048576, "max_tokens": 65536})
    );
}

// ---------------------------------------------------------------------------
// Token counting
// ---------------------------------------------------------------------------

#[test]
fn count_tokens_response_shapes() {
    assert_eq!(
        AnthropicCodec.encode_count_response(2095),
        Some(json!({"input_tokens": 2095}))
    );
    assert_eq!(
        AnthropicCodec.decode_count_response(&json!({"input_tokens": 2095})),
        Some(2095)
    );
    assert_eq!(
        AnthropicCodec.decode_count_response(&json!({"input_tokens": 12.0})),
        Some(12)
    );
    assert_eq!(
        AnthropicCodec.decode_count_response(&json!({"totalTokens": 5})),
        None
    );
    assert_eq!(AnthropicCodec.decode_count_response(&json!("x")), None);
}
