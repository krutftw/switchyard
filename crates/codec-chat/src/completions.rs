//! Shim for the legacy `POST /v1/completions` endpoint.
//!
//! The gateway serves text completions by running them as Chat Completions:
//! the request is rewritten into a one-message chat request, and the chat
//! response (or each stream chunk) is rewritten back into the
//! `text_completion` shape. These are pure functions over JSON so the server
//! can apply them around the normal Chat pipeline.

use serde_json::{Map, Value, json};

/// Prompt used when the client sent none. A chat model needs *something* to
/// answer, and an empty user message is rejected by several upstreams.
const EMPTY_PROMPT: &str = "Complete this:";

/// Fields with the same meaning in both APIs, copied when present and not
/// `null`.
const COPIED_FIELDS: &[&str] = &[
    "max_tokens",
    "temperature",
    "top_p",
    "n",
    "stop",
    "seed",
    "presence_penalty",
    "frequency_penalty",
    "logit_bias",
    "user",
    "stream",
    "stream_options",
];

/// Rewrites a `/v1/completions` request as a Chat Completions request.
///
/// * `prompt` becomes the single user message. An array of strings is joined
///   with newlines (batched prompts are not supported); an empty or missing
///   prompt becomes `"Complete this:"`.
/// * `logprobs` is an integer in the legacy API (how many alternatives to
///   return) and a boolean plus `top_logprobs` in Chat.
/// * `echo`, `suffix` and `best_of` have no Chat equivalent and are dropped.
pub fn completions_request_to_chat(body: &Value) -> Value {
    let mut out = Map::new();
    out.insert(
        "model".into(),
        body.get("model").cloned().unwrap_or_else(|| json!("")),
    );
    let prompt = match body.get("prompt") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    let prompt = if prompt.is_empty() {
        EMPTY_PROMPT.to_string()
    } else {
        prompt
    };
    out.insert(
        "messages".into(),
        json!([{"role": "user", "content": prompt}]),
    );
    for key in COPIED_FIELDS {
        if let Some(value) = body.get(*key).filter(|v| !v.is_null()) {
            out.insert((*key).into(), value.clone());
        }
    }
    match body.get("logprobs") {
        Some(Value::Bool(flag)) => {
            out.insert("logprobs".into(), json!(flag));
        }
        Some(Value::Number(n)) => {
            let wanted = n.as_u64().unwrap_or(0);
            out.insert("logprobs".into(), json!(true));
            if wanted > 0 {
                // Chat accepts at most 20 alternatives.
                out.insert("top_logprobs".into(), json!(wanted.min(20)));
            }
        }
        _ => {}
    }
    if out.get("logprobs") == Some(&Value::Bool(true))
        && !out.contains_key("top_logprobs")
        && let Some(top) = body.get("top_logprobs").filter(|v| v.is_number())
    {
        out.insert("top_logprobs".into(), top.clone());
    }
    Value::Object(out)
}

/// Text of a chat message or delta `content`: a string, or the text parts of
/// a part array.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(text.as_str()),
                other => other.get("text").and_then(Value::as_str),
            })
            .collect(),
        _ => String::new(),
    }
}

/// Copies the envelope fields shared by both shapes.
fn envelope(source: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(id) = source.get("id") {
        out.insert("id".into(), id.clone());
    }
    out.insert("object".into(), json!("text_completion"));
    for key in ["created", "model"] {
        if let Some(value) = source.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    out
}

fn finish_extras(source: &Value, out: &mut Map<String, Value>) {
    for key in ["system_fingerprint", "service_tier"] {
        if let Some(value) = source.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), value.clone());
        }
    }
}

/// Rewrites a `chat.completion` object as a `text_completion` object.
///
/// Every choice keeps its `index` and `finish_reason`; `text` is the message
/// content (the empty string for answers without text, such as tool calls).
/// Bodies that are not chat completions (an error envelope) are returned
/// unchanged.
pub fn chat_response_to_completions(body: &Value) -> Value {
    let Some(choices) = body.get("choices").and_then(Value::as_array) else {
        return body.clone();
    };
    let mut out = envelope(body);
    let choices: Vec<Value> = choices
        .iter()
        .enumerate()
        .map(|(position, choice)| {
            json!({
                "index": choice.get("index").cloned().unwrap_or_else(|| json!(position)),
                "text": content_text(choice.get("message").and_then(|m| m.get("content"))),
                "logprobs": choice.get("logprobs").cloned().unwrap_or(Value::Null),
                "finish_reason": choice.get("finish_reason").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    out.insert("choices".into(), Value::Array(choices));
    if let Some(usage) = body.get("usage").filter(|u| !u.is_null()) {
        out.insert("usage".into(), usage.clone());
    }
    finish_extras(body, &mut out);
    Value::Object(out)
}

/// Rewrites one `chat.completion.chunk` as a `text_completion` stream chunk.
///
/// Returns `None` for chunks that carry nothing a completions client can
/// use: no text, no finish reason and no usage (role-only chunks, tool-call
/// and reasoning deltas). Error frames are passed through unchanged.
pub fn chat_chunk_to_completions(chunk: &Value) -> Option<Value> {
    if chunk.get("error").is_some_and(|e| !e.is_null()) {
        return Some(chunk.clone());
    }
    let choices = chunk.get("choices").and_then(Value::as_array)?;
    let usage = chunk.get("usage").filter(|u| !u.is_null());
    let mut useful = usage.is_some();
    let choices: Vec<Value> = choices
        .iter()
        .enumerate()
        .filter_map(|(position, choice)| {
            let text = content_text(choice.get("delta").and_then(|d| d.get("content")));
            let finish = choice
                .get("finish_reason")
                .filter(|f| f.as_str().is_some_and(|s| !s.is_empty()));
            if text.is_empty() && finish.is_none() {
                return None;
            }
            useful = true;
            Some(json!({
                "index": choice.get("index").cloned().unwrap_or_else(|| json!(position)),
                "text": text,
                "logprobs": choice.get("logprobs").cloned().unwrap_or(Value::Null),
                "finish_reason": finish.cloned().unwrap_or(Value::Null),
            }))
        })
        .collect();
    if !useful {
        return None;
    }
    let mut out = envelope(chunk);
    out.insert("choices".into(), Value::Array(choices));
    if let Some(usage) = usage {
        out.insert("usage".into(), usage.clone());
    }
    finish_extras(chunk, &mut out);
    Some(Value::Object(out))
}
