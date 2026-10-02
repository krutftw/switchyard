//! Complete (non-streamed) responses: decoding an upstream's response object
//! and rendering the IR as a response object for a client. The item and
//! response-object builders are shared with the stream encoder so a streamed
//! and a non-streamed answer to the same request look identical.
//!
//! # Finish reason ↔ `status`
//!
//! | [`FinishReason`] | `status` | `incomplete_details.reason` / `error` |
//! |---|---|---|
//! | `Stop` | `completed` | |
//! | `ToolCalls` | `completed` | (the output holds call items) |
//! | `Refusal` | `completed` | (the output holds a `refusal` part; without one it is reported like `ContentFilter`) |
//! | `Length` | `incomplete` | `max_output_tokens` |
//! | `ContentFilter` | `incomplete` | `content_filter` |
//! | `PauseTurn` | `incomplete` | `pause_turn` |
//! | `ContextWindow` | `incomplete` | `model_context_window_exceeded` |
//! | `Other(r)` | `incomplete` | `r` |
//! | `Error` | `failed` | `error: {code: "server_error", message}` |
//!
//! Decoding inverts the table. A `completed` response is `ToolCalls` when it
//! contains a call item, `Refusal` when a refusal is its only visible
//! content, and `Stop` otherwise. `incomplete` without a reason is `Length`.
//! `failed` decodes to `Error` only when the response still carries output;
//! a failed response with nothing in it is an upstream error, not a response
//! (see [`decode_response`]).
//! `pause_turn` and `model_context_window_exceeded` are not vendor values;
//! they exist so Anthropic-specific outcomes survive the trip through a
//! Responses client (the vendor documents `reason` as an open enum).

use crate::common::{
    P, ToolIndex, annotation_from_citation, i64_field, id_base, non_empty, parts_from_output_item,
    response_id, signature_for_client, tool_item_id, unwrap_custom_input, usage_from_wire,
    usage_to_wire,
};
use crate::error::sanitize_message;
use serde_json::{Map, Value, json};
use switchyard_core::ir::{
    FinishReason, MediaPart, MediaSource, Part, Reasoning, Response, ToolCallKind,
};
use switchyard_core::util::{new_call_id, new_id, now_unix, str_field, u64_field};
use switchyard_core::{ClientCtx, CodecError, Usage};

// ---------------------------------------------------------------------------
// Finish reason mapping
// ---------------------------------------------------------------------------

/// The `status` a finish reason is reported as, with the
/// `incomplete_details.reason` when the status is `incomplete`.
pub(crate) fn status_for(finish: &FinishReason) -> (&'static str, Option<String>) {
    match finish {
        FinishReason::Stop | FinishReason::ToolCalls | FinishReason::Refusal => ("completed", None),
        FinishReason::Length => ("incomplete", Some("max_output_tokens".into())),
        FinishReason::ContentFilter => ("incomplete", Some("content_filter".into())),
        FinishReason::PauseTurn => ("incomplete", Some("pause_turn".into())),
        FinishReason::ContextWindow => ("incomplete", Some("model_context_window_exceeded".into())),
        FinishReason::Other(reason) => ("incomplete", Some(reason.clone())),
        FinishReason::Error => ("failed", None),
    }
}

/// What kinds of content a response holds; decides between the finish
/// reasons that share the `completed` status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ContentKinds {
    pub tool_calls: bool,
    pub text: bool,
    pub refusal: bool,
}

impl ContentKinds {
    pub(crate) fn of(parts: &[Part]) -> Self {
        let mut kinds = ContentKinds::default();
        for part in parts {
            kinds.note(part);
        }
        kinds
    }

    pub(crate) fn note(&mut self, part: &Part) {
        match part {
            Part::ToolCall(_) => self.tool_calls = true,
            Part::Text(text) if !text.text.is_empty() => self.text = true,
            Part::Refusal(_) => self.refusal = true,
            _ => {}
        }
    }
}

/// Inverse of [`status_for`].
pub(crate) fn finish_from(status: &str, reason: Option<&str>, kinds: ContentKinds) -> FinishReason {
    match status.trim().to_ascii_lowercase().as_str() {
        "incomplete" => match reason.map(str::trim).unwrap_or("") {
            "" | "max_output_tokens" | "max_tokens" | "length" => FinishReason::Length,
            "content_filter" => FinishReason::ContentFilter,
            "pause_turn" => FinishReason::PauseTurn,
            "model_context_window_exceeded" | "context_window_exceeded" => {
                FinishReason::ContextWindow
            }
            other => FinishReason::Other(other.to_string()),
        },
        "failed" => FinishReason::Error,
        // Not terminal outcomes of a generation the gateway waited for.
        other @ ("cancelled" | "queued" | "in_progress") => FinishReason::Other(other.to_string()),
        // `completed`, and compatible servers that omit the field.
        _ => {
            if kinds.tool_calls {
                FinishReason::ToolCalls
            } else if kinds.refusal && !kinds.text {
                FinishReason::Refusal
            } else {
                FinishReason::Stop
            }
        }
    }
}

// ---------------------------------------------------------------------------
// decode_response
// ---------------------------------------------------------------------------

/// `status` values of a response object.
const RESPONSE_STATUSES: &[&str] = &[
    "completed",
    "incomplete",
    "failed",
    "in_progress",
    "queued",
    "cancelled",
];

/// What a body says went wrong: the `error` member (object or string) with
/// its code, redacted. `None` when the body reports no error.
fn failure_text(body: &Value) -> Option<String> {
    let error = body.get("error").filter(|e| !e.is_null())?;
    let message = match error {
        Value::String(text) => text.trim().to_string(),
        other => non_empty(other, "message").unwrap_or("").to_string(),
    };
    let code = non_empty(error, "code").or_else(|| non_empty(error, "type"));
    let text = match (message.is_empty(), code) {
        (false, Some(code)) if !message.contains(code) => format!("{message} ({code})"),
        (false, _) => message,
        (true, Some(code)) => code.to_string(),
        (true, None) => return None,
    };
    Some(sanitize_message(&text))
}

/// Decodes an upstream response object. Also accepts a terminal stream event
/// (`{"type":"response.completed","response":{…}}`), which some upstreams
/// return as the body of a non-streaming call.
///
/// A response that failed without producing anything (`status: "failed"`, or
/// an `error` member, and no output) is not a response: the vendor reports
/// generation failures this way with HTTP 200, and the canonical response has
/// no slot for the error. It is returned as [`CodecError::InvalidUpstream`]
/// carrying the upstream's message and code, so the gateway treats the call
/// as the failure it is, exactly as it does for a `response.failed` event at
/// the head of a stream. A failed response that *did* produce output decodes
/// to that output with [`FinishReason::Error`], like a stream that broke
/// mid-flight.
pub(crate) fn decode_response(body: &Value) -> Result<Response, CodecError> {
    let body = match body.get("response") {
        Some(inner) if inner.is_object() && body.get("output").is_none() => inner,
        _ => body,
    };
    if !body.is_object() {
        return Err(CodecError::upstream("response body is not a JSON object"));
    }
    let status = str_field(body, "status")
        .map(|status| status.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let output = body.get("output").filter(|v| !v.is_null());
    let looks_like_response = str_field(body, "object") == Some("response")
        || RESPONSE_STATUSES.contains(&status.as_str())
        || body.get("output_text").is_some();
    let items: &[Value] = match output {
        Some(Value::Array(items)) => items,
        Some(_) => return Err(CodecError::upstream("`output` is not an array")),
        None if looks_like_response => &[],
        None => {
            // Something else entirely, typically an error envelope.
            let detail = failure_text(body)
                .or_else(|| {
                    ["message", "detail"]
                        .iter()
                        .find_map(|key| non_empty(body, key))
                        .map(sanitize_message)
                })
                .unwrap_or_else(|| "response object has no `output`".to_string());
            return Err(CodecError::upstream(detail));
        }
    };

    let mut parts = Vec::new();
    for item in items {
        parts_from_output_item(item, &mut parts);
    }
    if parts.is_empty() {
        // Minimal servers answer with the convenience field only.
        if let Some(text) = non_empty(body, "output_text") {
            parts.push(Part::text(text));
        }
    }
    if parts.is_empty() {
        let failure = failure_text(body);
        if status == "failed" || failure.is_some() {
            return Err(CodecError::upstream(failure.map_or_else(
                || "upstream response failed without an error message".to_string(),
                |text| format!("upstream response failed: {text}"),
            )));
        }
    }

    let reason = body
        .get("incomplete_details")
        .and_then(|d| str_field(d, "reason"));
    let finish = finish_from(
        if status.is_empty() {
            "completed"
        } else {
            status.as_str()
        },
        reason,
        ContentKinds::of(&parts),
    );
    Ok(Response {
        id: non_empty(body, "id")
            .map(str::to_string)
            .unwrap_or_else(|| new_id("resp_")),
        model: str_field(body, "model").unwrap_or("").to_string(),
        created: i64_field(body, "created_at")
            .or_else(|| i64_field(body, "created"))
            .unwrap_or(0),
        parts,
        finish,
        stop_sequence: None,
        usage: body
            .get("usage")
            .and_then(usage_from_wire)
            .unwrap_or_default(),
        service_tier: non_empty(body, "service_tier").map(str::to_string),
    })
}

// ---------------------------------------------------------------------------
// Output item builders
// ---------------------------------------------------------------------------

/// An `output_text` content part.
pub(crate) fn text_part(text: &str, annotations: Vec<Value>) -> Value {
    json!({"type": "output_text", "annotations": annotations, "logprobs": [], "text": text})
}

/// A `refusal` content part.
pub(crate) fn refusal_part(text: &str) -> Value {
    json!({"type": "refusal", "refusal": text})
}

/// An assistant `message` output item.
pub(crate) fn message_item(id: &str, status: &str, content: Vec<Value>) -> Value {
    json!({"id": id, "type": "message", "status": status, "content": content, "role": "assistant"})
}

/// A `reasoning` output item. `encrypted_content` is present only when there
/// is a blob to replay.
pub(crate) fn reasoning_item(id: &str, text: &str, encrypted: Option<&str>) -> Value {
    let mut item = Map::new();
    item.insert("id".into(), json!(id));
    item.insert("type".into(), json!("reasoning"));
    let summary = if text.is_empty() {
        Vec::new()
    } else {
        vec![json!({"type": "summary_text", "text": text})]
    };
    item.insert("summary".into(), Value::Array(summary));
    if let Some(encrypted) = encrypted {
        item.insert("encrypted_content".into(), json!(encrypted));
    }
    Value::Object(item)
}

/// The id a reasoning item is reported with: the provider's own `rs_…` id
/// when there is one, otherwise one derived from the response id.
pub(crate) fn reasoning_item_id(own: Option<&str>, base: &str, output_index: usize) -> String {
    match own.map(str::trim) {
        Some(id) if id.starts_with("rs_") => id.to_string(),
        _ => format!("rs_{base}_{output_index}"),
    }
}

/// The `encrypted_content` a client receives for a reasoning part.
pub(crate) fn reasoning_blob(reasoning: &Reasoning) -> Option<String> {
    reasoning
        .signature
        .as_ref()
        .map(|signature| signature_for_client(signature, reasoning.redacted))
}

/// How a tool call is presented to a Responses client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolView {
    pub item_id: String,
    pub call_id: String,
    /// Name as the client declared it.
    pub name: String,
    pub namespace: Option<String>,
    /// Rendered as `custom_tool_call` (free-form `input`).
    pub custom: bool,
    /// The upstream produced function-style JSON (`{"input": "…"}`) for a
    /// tool the client declared as custom; the raw input must be unwrapped.
    pub wrapped: bool,
}

impl ToolView {
    /// Restores the client's view of a call: `name` + `namespace` as declared
    /// and the custom flavour when the upstream had no notion of it.
    pub(crate) fn new(index: &ToolIndex, id: &str, name: &str, kind: ToolCallKind) -> Self {
        let identity = index.resolve(name);
        let declared_custom = identity.is_some_and(|i| i.custom);
        let native_custom = kind == ToolCallKind::Custom;
        let custom = native_custom || declared_custom;
        let call_id = if id.trim().is_empty() {
            new_call_id()
        } else {
            id.to_string()
        };
        ToolView {
            item_id: tool_item_id(&call_id, custom),
            call_id,
            name: identity.map_or_else(|| name.to_string(), |i| i.name.clone()),
            namespace: identity.and_then(|i| i.namespace.clone()),
            custom,
            wrapped: declared_custom && !native_custom,
        }
    }

    /// The final argument payload: raw input for custom tools, a JSON
    /// document for functions (`{}` when the model sent nothing and the call
    /// completed).
    pub(crate) fn payload(&self, arguments: &str, completed: bool) -> String {
        if self.custom {
            if self.wrapped {
                unwrap_custom_input(arguments)
            } else {
                arguments.to_string()
            }
        } else if arguments.trim().is_empty() && completed {
            "{}".to_string()
        } else {
            arguments.to_string()
        }
    }

    /// The call as a finished output item: `completed` with its final
    /// payload, or `incomplete` with whatever arguments had arrived when
    /// generation was cut short inside the call.
    pub(crate) fn finished_item(&self, arguments: &str, completed: bool) -> Value {
        let status = if completed { "completed" } else { "incomplete" };
        self.item(status, &self.payload(arguments, completed))
    }

    /// The call as an output item.
    pub(crate) fn item(&self, status: &str, payload: &str) -> Value {
        let mut item = Map::new();
        item.insert("id".into(), json!(self.item_id));
        item.insert(
            "type".into(),
            json!(if self.custom {
                "custom_tool_call"
            } else {
                "function_call"
            }),
        );
        item.insert("status".into(), json!(status));
        item.insert(
            if self.custom { "input" } else { "arguments" }.into(),
            json!(payload),
        );
        item.insert("call_id".into(), json!(self.call_id));
        item.insert("name".into(), json!(self.name));
        if let Some(namespace) = &self.namespace {
            item.insert("namespace".into(), json!(namespace));
        }
        Value::Object(item)
    }
}

/// A generated image as an `image_generation_call` item. Only inline images
/// can be expressed.
pub(crate) fn image_item(media: &MediaPart, base: &str, output_index: usize) -> Option<Value> {
    let MediaSource::Base64 { data } = &media.source else {
        return None;
    };
    let format = media
        .media_type
        .as_deref()
        .and_then(|t| t.strip_prefix("image/"))
        .unwrap_or("png");
    Some(json!({
        "id": format!("ig_{base}_{output_index}"),
        "type": "image_generation_call",
        "status": "completed",
        "output_format": format,
        "result": data,
    }))
}

/// Renders a part that is an output item by itself and needs no streaming:
/// provider-specific items of this protocol and generated images. `None` for
/// parts Responses cannot express.
pub(crate) fn whole_item(part: &Part, base: &str, output_index: usize) -> Option<Value> {
    match part {
        Part::Opaque(opaque) if opaque.origin == P && opaque.raw.is_object() => {
            Some(opaque.raw.clone())
        }
        Part::Image(media) => image_item(media, base, output_index),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The response object
// ---------------------------------------------------------------------------

/// Everything about a response object that does not depend on its outcome.
pub(crate) struct Shell<'a> {
    pub ctx: &'a ClientCtx,
    /// Final (`resp_…`) id.
    pub id: &'a str,
    pub created: i64,
    /// Model reported by the upstream; only used when the context has none.
    pub upstream_model: &'a str,
    pub service_tier: Option<&'a str>,
}

impl Shell<'_> {
    /// Builds the response object: outcome fields, then the request fields
    /// the vendor echoes — taken from the client's request when it is
    /// available and from the documented defaults otherwise.
    pub(crate) fn render(
        &self,
        finish: Option<&FinishReason>,
        output: Vec<Value>,
        usage: Option<&Usage>,
    ) -> Value {
        let request = &*self.ctx.request;
        let echo = |key: &str, default: Value| {
            request
                .get(key)
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or(default)
        };
        let (status, reason) = match finish {
            Some(finish) => status_for(finish),
            None => ("in_progress", None),
        };
        let error = if status == "failed" {
            json!({"code": "server_error", "message": "The model failed to generate a response."})
        } else {
            Value::Null
        };
        let model = if self.ctx.model.is_empty() {
            self.upstream_model
        } else {
            self.ctx.model.as_str()
        };

        let mut reasoning = match request.get("reasoning") {
            Some(Value::Object(reasoning)) => reasoning.clone(),
            _ => Map::new(),
        };
        for key in ["effort", "summary"] {
            reasoning.entry(key).or_insert(Value::Null);
        }
        let mut text = match request.get("text") {
            Some(Value::Object(text)) => text.clone(),
            _ => Map::new(),
        };
        text.entry("format")
            .or_insert_with(|| json!({"type": "text"}));

        let mut out = Map::new();
        out.insert("id".into(), json!(self.id));
        out.insert("object".into(), json!("response"));
        out.insert("created_at".into(), json!(self.created));
        out.insert("status".into(), json!(status));
        out.insert("background".into(), json!(false));
        out.insert("error".into(), error);
        out.insert(
            "incomplete_details".into(),
            match reason {
                Some(reason) => json!({"reason": reason}),
                None => Value::Null,
            },
        );
        out.insert("instructions".into(), echo("instructions", Value::Null));
        out.insert(
            "max_output_tokens".into(),
            echo("max_output_tokens", Value::Null),
        );
        out.insert("max_tool_calls".into(), echo("max_tool_calls", Value::Null));
        out.insert("model".into(), json!(model));
        out.insert("output".into(), Value::Array(output));
        out.insert(
            "parallel_tool_calls".into(),
            echo("parallel_tool_calls", json!(true)),
        );
        out.insert(
            "previous_response_id".into(),
            echo("previous_response_id", Value::Null),
        );
        out.insert(
            "prompt_cache_key".into(),
            echo("prompt_cache_key", Value::Null),
        );
        out.insert("reasoning".into(), Value::Object(reasoning));
        out.insert(
            "safety_identifier".into(),
            echo("safety_identifier", Value::Null),
        );
        out.insert(
            "service_tier".into(),
            match self.service_tier {
                Some(tier) => json!(tier),
                None => echo("service_tier", json!("default")),
            },
        );
        out.insert("store".into(), echo("store", json!(true)));
        out.insert("temperature".into(), echo("temperature", json!(1.0)));
        out.insert("text".into(), Value::Object(text));
        out.insert("tool_choice".into(), echo("tool_choice", json!("auto")));
        out.insert("tools".into(), echo("tools", json!([])));
        out.insert("top_logprobs".into(), echo("top_logprobs", json!(0)));
        out.insert("top_p".into(), echo("top_p", json!(1.0)));
        out.insert("truncation".into(), echo("truncation", json!("disabled")));
        out.insert(
            "usage".into(),
            match usage {
                Some(usage) => usage_to_wire(usage),
                None => Value::Null,
            },
        );
        out.insert("user".into(), echo("user", Value::Null));
        out.insert("metadata".into(), echo("metadata", json!({})));
        Value::Object(out)
    }
}

/// The finish reason as it can be shown to a Responses client. A refusal is
/// a `completed` response whose content is a `refusal` part; a refusal
/// *without* such a part (Anthropic's classifier stop) would read as a normal
/// completion, so it is reported as a content-filter stop instead.
pub(crate) fn presentable_finish(finish: &FinishReason, has_refusal_part: bool) -> FinishReason {
    match finish {
        FinishReason::Refusal if !has_refusal_part => FinishReason::ContentFilter,
        other => other.clone(),
    }
}

/// Whether an outcome leaves the trailing message unfinished.
pub(crate) fn cuts_output(finish: &FinishReason) -> bool {
    !matches!(
        finish,
        FinishReason::Stop | FinishReason::ToolCalls | FinishReason::Refusal
    )
}

// ---------------------------------------------------------------------------
// encode_response
// ---------------------------------------------------------------------------

/// Renders a complete response for a Responses client.
///
/// Output items, in part order: a run of adjacent text / refusal parts is one
/// `message` item with one content part per IR part; each reasoning part is a
/// `reasoning` item; each tool call a `function_call` (or `custom_tool_call`)
/// item whose `call_id` is the IR call id and whose item id is derived from
/// it. Ids that already have the vendor's shape (`resp_…`, `rs_…`) are kept.
/// When generation was cut short the trailing message or tool call is marked
/// `incomplete` (and the call's arguments are left as far as they got).
pub(crate) fn encode_response(response: &Response, ctx: &ClientCtx) -> Result<Value, CodecError> {
    let id = response_id(&response.id);
    let base = id_base(&id).to_string();
    let index = ToolIndex::from_request(&ctx.request);
    let has_refusal = response.parts.iter().any(|p| matches!(p, Part::Refusal(_)));
    let finish = presentable_finish(&response.finish, has_refusal);
    let cut = cuts_output(&finish);

    let mut output: Vec<Value> = Vec::new();
    let mut content: Option<Vec<Value>> = None;
    let flush = |output: &mut Vec<Value>, content: &mut Option<Vec<Value>>, status: &str| {
        if let Some(parts) = content.take() {
            let id = format!("msg_{base}_{}", output.len());
            output.push(message_item(&id, status, parts));
        }
    };
    let last = response.parts.len().saturating_sub(1);
    for (position, part) in response.parts.iter().enumerate() {
        match part {
            Part::Text(text) => {
                let annotations = text
                    .citations
                    .iter()
                    .filter_map(annotation_from_citation)
                    .collect();
                content
                    .get_or_insert_with(Vec::new)
                    .push(text_part(&text.text, annotations));
            }
            Part::Refusal(refusal) => {
                content
                    .get_or_insert_with(Vec::new)
                    .push(refusal_part(&refusal.text));
            }
            Part::Reasoning(reasoning) => {
                flush(&mut output, &mut content, "completed");
                let id = reasoning_item_id(reasoning.id.as_deref(), &base, output.len());
                output.push(reasoning_item(
                    &id,
                    &reasoning.text,
                    reasoning_blob(reasoning).as_deref(),
                ));
            }
            Part::ToolCall(call) => {
                flush(&mut output, &mut content, "completed");
                let view = ToolView::new(&index, &call.id, &call.name, call.kind);
                // Generation stopped inside this call: its arguments are
                // whatever had been produced, not a finished document.
                let completed = !(cut && position == last);
                output.push(view.finished_item(&call.arguments, completed));
            }
            other => {
                // Probe first: a part with no wire form must not split the
                // message, and the item's index is only known once the
                // message before it has been written.
                if whole_item(other, &base, 0).is_some() {
                    flush(&mut output, &mut content, "completed");
                    if let Some(item) = whole_item(other, &base, output.len()) {
                        output.push(item);
                    }
                }
            }
        }
    }
    flush(
        &mut output,
        &mut content,
        if cut { "incomplete" } else { "completed" },
    );

    let created = if response.created > 0 {
        response.created
    } else {
        now_unix()
    };
    let shell = Shell {
        ctx,
        id: &id,
        created,
        upstream_model: &response.model,
        service_tier: response.service_tier.as_deref(),
    };
    Ok(shell.render(Some(&finish), output, Some(&response.usage)))
}

// ---------------------------------------------------------------------------
// Model rewrite and token counting
// ---------------------------------------------------------------------------

/// Replaces the model name in a response object or in a stream event that
/// carries one (`response.created`, `response.completed`, …).
pub(crate) fn rewrite_response_model(payload: &mut Value, model: &str) {
    let Some(root) = payload.as_object_mut() else {
        return;
    };
    if root.get("model").is_some_and(Value::is_string) {
        root.insert("model".into(), json!(model));
    }
    if let Some(Value::Object(response)) = root.get_mut("response")
        && response.get("model").is_some_and(Value::is_string)
    {
        response.insert("model".into(), json!(model));
    }
}

/// `{"object":"response.input_tokens","input_tokens":N}`
pub(crate) fn encode_count_response(input_tokens: u64) -> Value {
    json!({"object": "response.input_tokens", "input_tokens": input_tokens})
}

/// Reads the count out of an `input_tokens` endpoint response. Compatible
/// servers that answer with a usage object are understood too.
pub(crate) fn decode_count_response(body: &Value) -> Option<u64> {
    u64_field(body, "input_tokens")
        .or_else(|| body.get("usage").and_then(|u| u64_field(u, "input_tokens")))
        .or_else(|| u64_field(body, "total_tokens"))
}
