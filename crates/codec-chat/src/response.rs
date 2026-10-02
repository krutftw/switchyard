//! Complete (non-streamed) Chat Completions responses: decoding an
//! upstream's `chat.completion` object into the IR and rendering an IR
//! response for a Chat client.

use crate::common::{
    ID_PREFIX, PROTOCOL, Side, arguments_text, citation_to_wire, citations_from_wire,
    client_response_id, finish_from_wire, finish_to_wire, i64_of, image_to_wire, images_from_wire,
    reasoning_from_message, reasoning_text, reconcile_finish, str_of, tool_call_from_wire,
    tool_call_to_wire, upstream_finish, usage_from_wire, usage_to_wire, write_reasoning_fields,
};
use serde_json::{Map, Value, json};
use switchyard_core::codec::ClientCtx;
use switchyard_core::error::CodecError;
use switchyard_core::ir::{
    FinishReason, MediaPart, Part, Reasoning, RefusalPart, Response, TextPart,
};
use switchyard_core::sig;
use switchyard_core::util::{new_call_id, new_id, now_unix};

// ---------------------------------------------------------------------------
// decode_response
// ---------------------------------------------------------------------------

/// Decodes an upstream `chat.completion` object.
///
/// Only the first choice is used: the IR models one candidate. Parts are
/// produced in the order reasoning, content, refusal, generated images, tool
/// calls. Every opaque blob is tagged as issued by a Chat upstream.
pub(crate) fn decode_response(body: &Value) -> Result<Response, CodecError> {
    let obj = body
        .as_object()
        .ok_or_else(|| CodecError::upstream("response body is not a JSON object"))?;
    let choices = match obj.get("choices") {
        Some(Value::Array(choices)) => choices,
        _ => {
            // Some servers answer 200 with an error envelope.
            if let Some(error) = obj.get("error").filter(|e| !e.is_null()) {
                let message = match error {
                    Value::String(text) => text.clone(),
                    other => str_of(other, "message")
                        .map(str::to_string)
                        .unwrap_or_else(|| other.to_string()),
                };
                return Err(CodecError::upstream(format!("error payload: {message}")));
            }
            return Err(CodecError::upstream("response has no `choices` array"));
        }
    };
    let choice = choices
        .iter()
        .find(|c| i64_of(c, "index").unwrap_or(0) == 0)
        .or_else(|| choices.first());

    let mut response = Response::new(
        str_of(body, "id")
            .map(str::to_string)
            .unwrap_or_else(|| new_id(ID_PREFIX)),
        str_of(body, "model").unwrap_or_default(),
    );
    response.created = i64_of(body, "created").unwrap_or(0);
    response.service_tier = str_of(body, "service_tier").map(str::to_string);
    response.usage = obj
        .get("usage")
        .and_then(usage_from_wire)
        .unwrap_or_default();

    let Some(choice) = choice else {
        // `choices: []` is how a few servers report "nothing was generated".
        return Ok(response);
    };
    let null = Value::Null;
    let message = choice
        .get("message")
        .filter(|m| m.is_object())
        // A handful of servers answer a non-streaming call with `delta`.
        .or_else(|| choice.get("delta").filter(|m| m.is_object()))
        .unwrap_or(&null);

    let mut parts: Vec<Part> = reasoning_from_message(message, Side::Upstream)
        .into_iter()
        .map(Part::Reasoning)
        .collect();
    let mut content = content_parts(message.get("content"));
    if content.is_empty()
        && let Some(text) = str_of(choice, "text")
    {
        // Legacy completion shape.
        content.push(Part::text(text));
    }
    if content.is_empty()
        && let Some(transcript) = message.get("audio").and_then(|a| str_of(a, "transcript"))
    {
        // Audio-output models answer with `content: null`; the transcript is
        // the part of the answer every protocol can carry.
        content.push(Part::text(transcript));
    }
    let citations = citations_from_wire(message.get("annotations"));
    if !citations.is_empty()
        && let Some(Part::Text(text)) = content.iter_mut().find(|p| matches!(p, Part::Text(_)))
    {
        text.citations.extend(citations);
    }
    parts.extend(content);
    if let Some(refusal) = str_of(message, "refusal") {
        parts.push(Part::Refusal(RefusalPart {
            text: refusal.to_string(),
        }));
    }
    parts.extend(images_from_wire(message.get("images")));
    if let Some(Value::Array(calls)) = message.get("tool_calls") {
        parts.extend(
            calls
                .iter()
                .filter_map(|tc| tool_call_from_wire(tc, Side::Upstream))
                .map(Part::ToolCall),
        );
    }
    if let Some(call) = message.get("function_call").filter(|f| f.is_object())
        && let Some(name) = str_of(call, "name")
    {
        // The deprecated single-call form has no id.
        parts.push(Part::tool_call(
            new_call_id(),
            name,
            arguments_text(call.get("arguments")),
        ));
    }

    // Same rule as the stream decoder: `stop` with tool calls is a tool turn,
    // unless a call's arguments are a cut-off document.
    response.finish = upstream_finish(
        choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(finish_from_wire),
        parts.iter().filter_map(|p| match p {
            Part::ToolCall(call) => Some((call.kind, call.arguments.as_str())),
            _ => None,
        }),
    );
    if response.finish == FinishReason::Stop {
        // vLLM reports the stop string that matched as `stop_reason`.
        response.stop_sequence = str_of(choice, "stop_reason")
            .or_else(|| str_of(choice, "stop_sequence"))
            .map(str::to_string);
    }
    response.parts = parts;
    Ok(response)
}

/// Decodes assistant `content`: a string, `null`, or (non-standard) an
/// array of typed parts. Mistral-style `thinking` items become reasoning.
fn content_parts(content: Option<&Value>) -> Vec<Part> {
    match content {
        Some(Value::String(text)) if !text.is_empty() => vec![Part::text(text.as_str())],
        Some(Value::Array(items)) => items.iter().filter_map(content_item).collect(),
        _ => Vec::new(),
    }
}

fn content_item(item: &Value) -> Option<Part> {
    if let Value::String(text) = item {
        return (!text.is_empty()).then(|| Part::text(text.as_str()));
    }
    let kind = item.get("type").and_then(Value::as_str).unwrap_or("text");
    match kind {
        "text" | "output_text" => {
            let text = str_of(item, "text")?;
            Some(Part::Text(TextPart {
                text: text.to_string(),
                cache_control: None,
                citations: citations_from_wire(item.get("annotations")),
                signature: None,
            }))
        }
        "refusal" => str_of(item, "refusal").map(|text| {
            Part::Refusal(RefusalPart {
                text: text.to_string(),
            })
        }),
        "thinking" | "reasoning" => {
            let text = reasoning_text(
                item.get("thinking")
                    .or_else(|| item.get("reasoning"))
                    .or_else(|| item.get("text")),
            );
            (!text.is_empty()).then(|| {
                Part::Reasoning(Reasoning {
                    text,
                    ..Reasoning::default()
                })
            })
        }
        "image_url" => {
            let spec = item.get("image_url")?;
            let url = spec.as_str().or_else(|| str_of(spec, "url"))?;
            Some(Part::Image(MediaPart::from_url(url)))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// encode_response
// ---------------------------------------------------------------------------

/// Renders an IR response as a `chat.completion` object.
///
/// * all text parts are concatenated into `content` (`null` when the turn is
///   only tool calls or a refusal);
/// * reasoning text goes to `reasoning_content`, blobs to `reasoning_details`
///   (wrapped with [`sig::encode_for_client`] when another vendor issued them);
/// * generated images go to the `images` extension array;
/// * audio, documents and provider-specific blocks have no Chat
///   representation and are dropped.
pub(crate) fn encode_response(response: &Response, ctx: &ClientCtx) -> Result<Value, CodecError> {
    let mut text = String::new();
    let mut has_text = false;
    let mut refusal = String::new();
    let mut calls: Vec<Value> = Vec::new();
    let mut reasoning: Vec<&Reasoning> = Vec::new();
    let mut annotations: Vec<Value> = Vec::new();
    let mut images: Vec<Value> = Vec::new();
    for part in &response.parts {
        match part {
            Part::Text(t) => {
                let offset = text.chars().count() as u64;
                text.push_str(&t.text);
                has_text = true;
                annotations.extend(
                    t.citations
                        .iter()
                        .filter_map(|c| citation_to_wire(c, offset)),
                );
            }
            Part::Refusal(r) => refusal.push_str(&r.text),
            Part::ToolCall(call) => {
                let signature = call
                    .signature
                    .as_ref()
                    .map(|s| sig::encode_for_client(s, PROTOCOL));
                calls.push(tool_call_to_wire(call, signature));
            }
            Part::Reasoning(r) => reasoning.push(r),
            Part::Image(media) => images.extend(image_to_wire(media, images.len())),
            Part::Audio(_) | Part::Document(_) | Part::ToolResult(_) | Part::Opaque(_) => {}
        }
    }

    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    let content = if has_text || (calls.is_empty() && refusal.is_empty()) {
        json!(text)
    } else {
        Value::Null
    };
    message.insert("content".into(), content);
    write_reasoning_fields(&mut message, reasoning.into_iter(), "\n\n", |s| {
        Some(sig::encode_for_client(s, PROTOCOL))
    });
    message.insert(
        "refusal".into(),
        if refusal.is_empty() {
            Value::Null
        } else {
            json!(refusal)
        },
    );
    let has_calls = !calls.is_empty();
    if has_calls {
        message.insert("tool_calls".into(), Value::Array(calls));
    }
    if !annotations.is_empty() {
        message.insert("annotations".into(), Value::Array(annotations));
    }
    if !images.is_empty() {
        message.insert("images".into(), Value::Array(images));
    }

    let finish = reconcile_finish(response.finish.clone(), has_calls);
    let mut body = Map::new();
    body.insert("id".into(), json!(client_response_id(&response.id)));
    body.insert("object".into(), json!("chat.completion"));
    body.insert(
        "created".into(),
        json!(if response.created > 0 {
            response.created
        } else {
            now_unix()
        }),
    );
    body.insert(
        "model".into(),
        json!(if ctx.model.is_empty() {
            &response.model
        } else {
            &ctx.model
        }),
    );
    body.insert(
        "choices".into(),
        json!([{
            "index": 0,
            "message": Value::Object(message),
            "logprobs": null,
            "finish_reason": finish_to_wire(&finish),
        }]),
    );
    body.insert("usage".into(), usage_to_wire(&response.usage));
    if let Some(tier) = &response.service_tier {
        body.insert("service_tier".into(), json!(tier));
    }
    Ok(Value::Object(body))
}

/// Replaces the top-level `model` of a `chat.completion` object or of a
/// single `chat.completion.chunk`. Payloads without a string `model` (error
/// frames, foreign events) are left untouched.
pub(crate) fn rewrite_response_model(payload: &mut Value, model: &str) {
    if let Some(obj) = payload.as_object_mut()
        && obj.get("model").is_some_and(Value::is_string)
    {
        obj.insert("model".into(), json!(model));
    }
}
