//! The legacy `/v1/completions` shim: pure JSON rewrites around the Chat
//! pipeline.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_chat::{
    ChatCodec, chat_chunk_to_completions, chat_response_to_completions, completions_request_to_chat,
};
use switchyard_core::codec::{Codec, RequestPath};

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

#[test]
fn request_prompt_becomes_a_user_message() {
    let chat = completions_request_to_chat(&json!({
        "model": "gpt-3.5-turbo-instruct",
        "prompt": "Once upon a time",
        "max_tokens": 64,
        "temperature": 0.7,
        "top_p": 0.9,
        "n": 1,
        "stop": ["\n"],
        "seed": 3,
        "presence_penalty": 0.1,
        "frequency_penalty": 0.2,
        "user": "u1",
        "stream": true,
        "stream_options": {"include_usage": true},
        // No Chat equivalent.
        "echo": true,
        "suffix": " The end.",
        "best_of": 3
    }));
    assert_eq!(
        chat,
        json!({
            "model": "gpt-3.5-turbo-instruct",
            "messages": [{"role": "user", "content": "Once upon a time"}],
            "max_tokens": 64,
            "temperature": 0.7,
            "top_p": 0.9,
            "n": 1,
            "stop": ["\n"],
            "seed": 3,
            "presence_penalty": 0.1,
            "frequency_penalty": 0.2,
            "user": "u1",
            "stream": true,
            "stream_options": {"include_usage": true}
        })
    );
    // The result is a request the codec accepts.
    let request = ChatCodec
        .decode_request(&chat, &RequestPath::default())
        .expect("a valid chat request");
    assert_eq!(request.messages[0].text(), "Once upon a time");
    assert_eq!(request.max_output_tokens, Some(64));
    assert!(request.stream);
}

#[test]
fn request_prompt_shapes() {
    let prompt = |prompt: Value| {
        completions_request_to_chat(&json!({"model": "m", "prompt": prompt}))["messages"][0]
            ["content"]
            .clone()
    };
    assert_eq!(prompt(json!("hello")), json!("hello"));
    // Batched prompts are not supported: the strings are joined.
    assert_eq!(
        prompt(json!(["line one", "line two"])),
        json!("line one\nline two")
    );
    // A chat model needs something to answer.
    assert_eq!(prompt(json!("")), json!("Complete this:"));
    assert_eq!(prompt(Value::Null), json!("Complete this:"));
    assert_eq!(prompt(json!([])), json!("Complete this:"));
    assert_eq!(
        completions_request_to_chat(&json!({"model": "m"}))["messages"],
        json!([{"role": "user", "content": "Complete this:"}])
    );
}

#[test]
fn request_null_fields_are_not_copied() {
    assert_eq!(
        completions_request_to_chat(&json!({
            "model": "m", "prompt": "x", "max_tokens": null, "stop": null, "stream": null
        })),
        json!({"model": "m", "messages": [{"role": "user", "content": "x"}]})
    );
}

#[test]
fn request_logprobs_integer_becomes_the_chat_pair() {
    let convert = |body: Value| completions_request_to_chat(&body);
    let base = |extra: Value| {
        let mut body = json!({"model": "m", "prompt": "x"});
        for (key, value) in extra.as_object().expect("an object") {
            body[key] = value.clone();
        }
        convert(body)
    };
    let chat = base(json!({"logprobs": 3}));
    assert_eq!(
        (chat["logprobs"].clone(), chat["top_logprobs"].clone()),
        (json!(true), json!(3))
    );
    let chat = base(json!({"logprobs": 0}));
    assert_eq!(chat["logprobs"], json!(true));
    assert!(chat.get("top_logprobs").is_none());
    let chat = base(json!({"logprobs": true, "top_logprobs": 5}));
    assert_eq!(
        (chat["logprobs"].clone(), chat["top_logprobs"].clone()),
        (json!(true), json!(5))
    );
    let chat = base(json!({"logprobs": false, "top_logprobs": 5}));
    assert_eq!(chat["logprobs"], json!(false));
    assert!(chat.get("top_logprobs").is_none());
    let chat = base(json!({"logprobs": null}));
    assert!(chat.get("logprobs").is_none());
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

#[test]
fn response_is_rewritten_as_a_text_completion() {
    let completion = chat_response_to_completions(&json!({
        "id": "chatcmpl-abc",
        "object": "chat.completion",
        "created": 1741570002,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": " there was a fox.", "refusal": null},
            "logprobs": null,
            "finish_reason": "length"
        }],
        "usage": {"prompt_tokens": 4, "completion_tokens": 6, "total_tokens": 10},
        "system_fingerprint": "fp_1"
    }));
    assert_eq!(
        completion,
        json!({
            "id": "chatcmpl-abc",
            "object": "text_completion",
            "created": 1741570002,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "text": " there was a fox.",
                "logprobs": null,
                "finish_reason": "length"
            }],
            "usage": {"prompt_tokens": 4, "completion_tokens": 6, "total_tokens": 10},
            "system_fingerprint": "fp_1"
        })
    );
    assert_eq!(
        completion
            .as_object()
            .expect("object")
            .keys()
            .collect::<Vec<_>>(),
        [
            "id",
            "object",
            "created",
            "model",
            "choices",
            "usage",
            "system_fingerprint"
        ]
    );
}

#[test]
fn response_choices_without_text() {
    let completion = chat_response_to_completions(&json!({
        "id": "x", "created": 1, "model": "m",
        "choices": [
            {"index": 0, "message": {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]}, "finish_reason": "tool_calls"},
            {"index": 1, "message": {"content": [
                {"type": "text", "text": "part "}, {"type": "text", "text": "array"}
            ]}, "finish_reason": "stop"},
            {"message": {"content": "no index"}}
        ]
    }));
    assert_eq!(
        completion["choices"],
        json!([
            // `text` is a required string in the completions schema.
            {"index": 0, "text": "", "logprobs": null, "finish_reason": "tool_calls"},
            {"index": 1, "text": "part array", "logprobs": null, "finish_reason": "stop"},
            {"index": 2, "text": "no index", "logprobs": null, "finish_reason": null}
        ])
    );
    assert!(completion.get("usage").is_none());

    assert_eq!(
        chat_response_to_completions(&json!({"id": "x", "model": "m", "choices": []}))["choices"],
        json!([])
    );
}

#[test]
fn response_error_bodies_pass_through() {
    let error =
        json!({"error": {"message": "boom", "type": "server_error", "param": null, "code": null}});
    assert_eq!(chat_response_to_completions(&error), error);
}

// ---------------------------------------------------------------------------
// Stream chunks
// ---------------------------------------------------------------------------

fn chat_chunk(delta: Value, finish: Value) -> Value {
    json!({
        "id": "chatcmpl-abc",
        "object": "chat.completion.chunk",
        "created": 1741570002,
        "model": "gpt-4o",
        "choices": [{"index": 0, "delta": delta, "logprobs": null, "finish_reason": finish}]
    })
}

#[test]
fn chunk_with_text_is_rewritten() {
    assert_eq!(
        chat_chunk_to_completions(&chat_chunk(json!({"content": " there"}), Value::Null)),
        Some(json!({
            "id": "chatcmpl-abc",
            "object": "text_completion",
            "created": 1741570002,
            "model": "gpt-4o",
            "choices": [{"index": 0, "text": " there", "logprobs": null, "finish_reason": null}]
        }))
    );
}

#[test]
fn chunk_with_finish_reason_is_kept_with_empty_text() {
    assert_eq!(
        chat_chunk_to_completions(&chat_chunk(json!({}), json!("stop"))),
        Some(json!({
            "id": "chatcmpl-abc",
            "object": "text_completion",
            "created": 1741570002,
            "model": "gpt-4o",
            // `null` while streaming, the reason at the end; never "".
            "choices": [{"index": 0, "text": "", "logprobs": null, "finish_reason": "stop"}]
        }))
    );
}

#[test]
fn chunks_a_completions_client_cannot_use_are_dropped() {
    for delta in [
        json!({"role": "assistant", "content": ""}),
        json!({"role": "assistant"}),
        json!({"reasoning_content": "thinking"}),
        json!({"tool_calls": [{"index": 0, "id": "c", "type": "function",
                               "function": {"name": "f", "arguments": ""}}]}),
        json!({"content": null}),
        json!({}),
    ] {
        assert_eq!(
            chat_chunk_to_completions(&chat_chunk(delta, Value::Null)),
            None
        );
    }
    // `usage: null` on ordinary chunks does not make them useful either.
    let mut chunk = chat_chunk(json!({"role": "assistant", "content": ""}), Value::Null);
    chunk["usage"] = Value::Null;
    assert_eq!(chat_chunk_to_completions(&chunk), None);
    // Not a chunk at all.
    assert_eq!(chat_chunk_to_completions(&json!({"object": "ping"})), None);
    assert_eq!(chat_chunk_to_completions(&json!("[DONE]")), None);
}

#[test]
fn usage_only_chunk_is_kept() {
    let chunk = json!({
        "id": "chatcmpl-abc", "object": "chat.completion.chunk", "created": 1741570002,
        "model": "gpt-4o", "choices": [],
        "usage": {"prompt_tokens": 4, "completion_tokens": 6, "total_tokens": 10}
    });
    assert_eq!(
        chat_chunk_to_completions(&chunk),
        Some(json!({
            "id": "chatcmpl-abc",
            "object": "text_completion",
            "created": 1741570002,
            "model": "gpt-4o",
            "choices": [],
            "usage": {"prompt_tokens": 4, "completion_tokens": 6, "total_tokens": 10}
        }))
    );
}

#[test]
fn error_frames_pass_through() {
    let error =
        json!({"error": {"message": "boom", "type": "server_error", "param": null, "code": null}});
    assert_eq!(chat_chunk_to_completions(&error), Some(error));
}

#[test]
fn shim_composes_with_the_stream_encoder() {
    use switchyard_core::codec::ClientCtx;
    use switchyard_core::ir::{FinishReason, Part, Response};
    use switchyard_core::stream::response_to_events;

    let mut response = Response::new("chatcmpl-abc", "gpt-4o");
    response.created = 1741570002;
    response.parts = vec![Part::reasoning("hidden"), Part::text("a fox.")];
    response.finish = FinishReason::Stop;
    let mut encoder = ChatCodec.stream_encoder(&ClientCtx::new("gpt-4o"));
    let mut wire = Vec::new();
    for event in response_to_events(&response) {
        wire.extend(encoder.encode(&event));
    }
    wire.extend(encoder.finish());
    let texts: Vec<Value> = wire
        .iter()
        .filter(|e| !e.is_done_marker())
        .filter_map(|e| serde_json::from_str::<Value>(&e.data).ok())
        .filter_map(|chunk| chat_chunk_to_completions(&chunk))
        .map(|chunk| chunk["choices"][0].clone())
        .collect();
    assert_eq!(
        texts,
        vec![
            json!({"index": 0, "text": "a fox.", "logprobs": null, "finish_reason": null}),
            json!({"index": 0, "text": "", "logprobs": null, "finish_reason": "stop"}),
        ]
    );
}
