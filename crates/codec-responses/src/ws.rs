//! Pure helpers for the Responses WebSocket endpoint.
//!
//! The gateway keeps a per-connection transcript so stateless upstreams can
//! serve incremental turns (see `docs/DESIGN.md` §10). Everything here works
//! on raw input items and has no connection state of its own.

use crate::common::{non_empty, type_of, usage_to_wire};
use crate::error::redact_secrets;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use switchyard_core::Usage;
use switchyard_core::util::new_id;

fn is_tool_call(item: &Value) -> bool {
    matches!(type_of(item), "function_call" | "custom_tool_call")
}

fn is_tool_output(item: &Value) -> bool {
    matches!(
        type_of(item),
        "function_call_output" | "custom_tool_call_output"
    )
}

fn call_id(item: &Value) -> &str {
    non_empty(item, "call_id").unwrap_or("")
}

fn item_id(item: &Value) -> &str {
    non_empty(item, "id").unwrap_or("")
}

/// Removes items that repeat an `id`. The last occurrence of an id is the
/// one kept — except that a tool call whose `call_id` is answered by an
/// output item is never displaced by a same-id item that is not, because the
/// upstream would then reject the orphaned output.
fn dedupe_by_id(items: Vec<Value>) -> Vec<Value> {
    let referenced: HashSet<String> = items
        .iter()
        .filter(|item| is_tool_output(item))
        .map(call_id)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    // id → (index of the occurrence to keep, whether that one is referenced)
    let mut keep: HashMap<String, (usize, bool)> = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        let id = item_id(item);
        if id.is_empty() {
            continue;
        }
        let is_referenced = referenced.contains(call_id(item));
        match keep.get_mut(id) {
            None => {
                keep.insert(id.to_string(), (index, is_referenced));
            }
            Some(kept) => {
                if is_referenced || !kept.1 {
                    *kept = (index, is_referenced);
                }
            }
        }
    }
    items
        .into_iter()
        .enumerate()
        .filter(|(index, item)| {
            let id = item_id(item);
            id.is_empty() || keep.get(id).is_some_and(|kept| kept.0 == *index)
        })
        .map(|(_, item)| item)
        .collect()
}

/// Builds the upstream input of a follow-up turn on a stateless upstream:
/// the previous turn's input, the previous response's output and the new
/// input, concatenated and de-duplicated.
///
/// * A tool call (`function_call` / `custom_tool_call`) whose non-empty
///   `call_id` already appeared is dropped: the first occurrence wins.
/// * Items sharing a non-empty `id` collapse to the last occurrence, unless
///   that would replace a tool call that an output item refers to with one
///   that nothing refers to.
///
/// Items are otherwise kept verbatim and in order.
pub fn merge_transcript(
    prev_input: &[Value],
    prev_output: &[Value],
    new_input: &[Value],
) -> Vec<Value> {
    let mut seen_calls: HashSet<&str> = HashSet::new();
    let merged: Vec<Value> = prev_input
        .iter()
        .chain(prev_output)
        .chain(new_input)
        .filter(|item| {
            // Copy the reference out so the id borrows from the slice, not
            // from this closure's argument.
            let item: &Value = item;
            if !is_tool_call(item) {
                return true;
            }
            let id = call_id(item);
            id.is_empty() || seen_calls.insert(id)
        })
        .cloned()
        .collect();
    dedupe_by_id(merged)
}

/// Whether a follow-up input (sent without `previous_response_id`) is a
/// complete transcript rather than an increment: it replays model output —
/// an assistant message or a tool call. Appending such an input to the
/// stored transcript would duplicate turns, so it replaces it instead.
///
/// A message may omit `type` (the protocol's short message form), so an item
/// with `role: "assistant"` and no type counts as an assistant message too.
pub fn is_transcript_replacement(input: &[Value]) -> bool {
    input.iter().any(|item| match type_of(item) {
        "function_call" | "custom_tool_call" => true,
        "message" | "" => non_empty(item, "role") == Some("assistant"),
        _ => false,
    })
}

/// Makes tool calls and tool outputs pair up, the way a stateless Responses
/// upstream insists: a `function_call` / `custom_tool_call` without an output
/// in the same input is dropped, and so is an output whose call is missing.
///
/// Calls without a `call_id` are dropped. An output without a `call_id` is
/// kept only when it is a `function_call_output` carrying a `name` (a
/// standalone named result some clients send on purpose). Items sharing an
/// `id` are then collapsed as in [`merge_transcript`]. All other items pass
/// through untouched.
pub fn repair_tool_pairs(input: Vec<Value>) -> Vec<Value> {
    let mut calls: HashSet<String> = HashSet::new();
    let mut outputs: HashSet<String> = HashSet::new();
    for item in &input {
        let id = call_id(item);
        if id.is_empty() {
            continue;
        }
        if is_tool_call(item) {
            calls.insert(id.to_string());
        } else if is_tool_output(item) {
            outputs.insert(id.to_string());
        }
    }
    let kept: Vec<Value> = input
        .into_iter()
        .filter(|item| {
            let id = call_id(item);
            if is_tool_output(item) {
                if id.is_empty() {
                    return type_of(item) == "function_call_output"
                        && non_empty(item, "name").is_some();
                }
                calls.contains(id)
            } else if is_tool_call(item) {
                !id.is_empty() && outputs.contains(id)
            } else {
                true
            }
        })
        .collect();
    dedupe_by_id(kept)
}

/// The error frame of the Responses WebSocket endpoint:
/// `{"type":"error","status":N,"error":{"message","type","code"?,"param"?}}`.
///
/// `error.type` follows the HTTP status the failure would have had; `code`
/// falls back to the status' customary code when the caller has none. A
/// status outside 400..=599 is reported as 500. Credentials an upstream may
/// have echoed into `message` (`Bearer …`, `api_key=…`) are redacted.
pub fn ws_error_frame(
    status: u16,
    message: &str,
    code: Option<&str>,
    param: Option<&str>,
) -> Value {
    let status = if (400..=599).contains(&status) {
        status
    } else {
        500
    };
    let (kind, default_code) = match status {
        401 => ("authentication_error", Some("invalid_api_key")),
        403 => ("permission_error", Some("insufficient_quota")),
        404 => ("invalid_request_error", Some("model_not_found")),
        408 => ("server_error", Some("request_timeout")),
        429 => ("rate_limit_error", Some("rate_limit_exceeded")),
        500..=599 => ("server_error", Some("internal_server_error")),
        _ => ("invalid_request_error", None),
    };
    let mut error = Map::new();
    error.insert("message".into(), json!(redact_secrets(message)));
    error.insert("type".into(), json!(kind));
    if let Some(code) = code
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .or(default_code)
    {
        error.insert("code".into(), json!(code));
    }
    if let Some(param) = param.map(str::trim).filter(|p| !p.is_empty()) {
        error.insert("param".into(), json!(param));
    }
    json!({"type": "error", "status": status, "error": error})
}

/// The two frames that answer a prewarm request (`"generate": false`)
/// locally: `response.created` and `response.completed` for an empty
/// response with zero usage and a synthetic `resp_prewarm_…` id. `now` is
/// the creation time in unix seconds; `model` is echoed when non-empty.
pub fn prewarm_frames(model: &str, now: i64) -> (Value, Value) {
    let id = new_id("resp_prewarm_");
    let frame = |kind: &str, sequence: u64, status: &str, usage: Option<Value>| {
        let mut response = Map::new();
        response.insert("id".into(), json!(id));
        response.insert("object".into(), json!("response"));
        response.insert("created_at".into(), json!(now));
        response.insert("status".into(), json!(status));
        response.insert("background".into(), json!(false));
        response.insert("error".into(), Value::Null);
        response.insert("output".into(), json!([]));
        if let Some(usage) = usage {
            response.insert("usage".into(), usage);
        }
        if !model.trim().is_empty() {
            response.insert("model".into(), json!(model));
        }
        json!({"type": kind, "sequence_number": sequence, "response": response})
    };
    (
        frame("response.created", 0, "in_progress", None),
        frame(
            "response.completed",
            1,
            "completed",
            Some(usage_to_wire(&Usage::default())),
        ),
    )
}
