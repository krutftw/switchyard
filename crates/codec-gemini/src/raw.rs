//! Inspecting and patching raw Gemini bodies without decoding them: the
//! helpers the gateway uses on the same-protocol passthrough path, plus the
//! Vertex AI body adaptation.

use crate::parts::{
    SKIP_SIGNATURE, call_name, is_bypass_signature, is_metadata_only, raw_signature,
    remove_signature, signature_from_client,
};
use crate::util::{num_u64, pick, pick_str, strip_models_prefix};
use serde_json::{Map, Value, json};
use switchyard_core::{CodecError, Protocol, RequestMeta, RequestPath, UpstreamCtx};

const FUNCTION_CALL: [&str; 2] = ["functionCall", "function_call"];
const FUNCTION_RESPONSE: [&str; 2] = ["functionResponse", "function_response"];
/// The wrapper of the `countTokens` form that counts system instructions and
/// tools as well.
const COUNT_WRAPPER: [&str; 2] = ["generateContentRequest", "generate_content_request"];

/// Model and stream flag of a request. Gemini carries both in the URL
/// (`/v1beta/models/{model}:streamGenerateContent`); a body `model`
/// (`"models/x"` or `"x"`) and a body `stream` flag are accepted as fallbacks
/// for clients that reach the gateway some other way.
pub(crate) fn request_meta(
    body: &Value,
    path: &RequestPath<'_>,
) -> Result<RequestMeta, CodecError> {
    if !body.is_object() {
        return Err(CodecError::invalid(
            "the request body must be a JSON object",
        ));
    }
    fn named_in(holder: &Value) -> Option<&str> {
        holder
            .get("model")
            .and_then(Value::as_str)
            .map(|m| strip_models_prefix(m.trim()))
            .filter(|m| !m.is_empty())
    }
    let from_path = path.model.map(str::trim).filter(|m| !m.is_empty());
    // A wrapped `countTokens` request names the model inside the wrapper.
    let from_body = || named_in(request_root(body)).or_else(|| named_in(body));
    let Some(model) = from_path.or_else(from_body) else {
        return Err(CodecError::invalid_param(
            "model",
            "no model given: Gemini requests name the model in the URL (`/v1beta/models/{model}:generateContent`)",
        ));
    };
    let stream = path
        .stream
        .or_else(|| body.get("stream").and_then(Value::as_bool))
        .unwrap_or(false);
    Ok(RequestMeta {
        model: model.to_string(),
        stream,
    })
}

/// Keeps a body `model` field consistent with the routed model: the top-level
/// one and the one inside a `countTokens` `generateContentRequest` wrapper
/// (which the API checks against the URL). Bodies without one (the normal
/// case) are left alone: the URL carries the model.
pub(crate) fn set_request_model(body: &mut Value, model: &str) {
    let bare = strip_models_prefix(model);
    let rename = |holder: &mut Value| {
        if let Some(Value::String(current)) = holder.get_mut("model") {
            *current = if current.starts_with("models/") {
                format!("models/{bare}")
            } else {
                bare.to_string()
            };
        }
    };
    rename(body);
    for key in COUNT_WRAPPER {
        if let Some(inner) = body.get_mut(key) {
            rename(inner);
        }
    }
}

/// The object holding the request fields (`contents`, `tools`, …): the body
/// itself or, for the wrapped form of a `countTokens` request
/// (`{"generateContentRequest": {"model": …, "contents": …}}`), the request
/// inside the wrapper. The API ignores a top-level `contents` when the
/// wrapper is present, and so does this codec.
pub(crate) fn request_root(body: &Value) -> &Value {
    match pick(body, &COUNT_WRAPPER) {
        Some(inner) if inner.is_object() => inner,
        _ => body,
    }
}

/// Some relays wrap every payload as `{"response": {...}}`.
pub(crate) fn unwrap_envelope(value: &Value) -> &Value {
    match value.get("response") {
        Some(inner)
            if inner.is_object()
                && [
                    "candidates",
                    "responseId",
                    "usageMetadata",
                    "promptFeedback",
                    "modelVersion",
                ]
                .iter()
                .any(|key| inner.get(*key).is_some()) =>
        {
            inner
        }
        _ => value,
    }
}

/// Replaces `modelVersion` in a response body, a stream chunk, or the JSON
/// array form of a stream.
pub(crate) fn rewrite_response_model(payload: &mut Value, model: &str) {
    match payload {
        Value::Array(chunks) => chunks
            .iter_mut()
            .for_each(|chunk| rewrite_response_model(chunk, model)),
        Value::Object(map) => {
            if let Some(Value::String(version)) = map.get_mut("modelVersion") {
                *version = model.to_string();
            }
            if let Some(inner @ Value::Object(_)) = map.get_mut("response") {
                rewrite_response_model(inner, model);
            }
        }
        _ => {}
    }
}

fn has_part(content: &Value, spellings: &[&str]) -> bool {
    pick(content, &["parts"])
        .and_then(Value::as_array)
        .is_some_and(|parts| parts.iter().any(|part| pick(part, spellings).is_some()))
}

fn role_of(content: &Value) -> String {
    pick_str(content, &["role"])
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Gives contents with a role Gemini does not know a valid one: `assistant`
/// becomes `model`; `function` / `tool` become `user`; anything else follows
/// its parts (a function response is a user turn, a function call a model
/// turn) or alternates with the previous turn. `user` / `model` and contents
/// without a role (Gemini reads those as user turns) are never changed.
fn repair_roles(contents: &mut [Value]) {
    let mut previous = String::new();
    for content in contents.iter_mut() {
        let role = role_of(content);
        let effective = match role.as_str() {
            "user" | "model" | "" => role.clone(),
            "assistant" => "model".to_string(),
            "function" | "tool" => "user".to_string(),
            _ if has_part(content, &FUNCTION_RESPONSE) => "user".to_string(),
            _ if has_part(content, &FUNCTION_CALL) => "model".to_string(),
            _ if previous == "user" => "model".to_string(),
            _ => "user".to_string(),
        };
        if effective != role
            && let Some(map) = content.as_object_mut()
        {
            map.insert("role".to_string(), Value::String(effective.clone()));
        }
        previous = if effective.is_empty() {
            "user".to_string()
        } else {
            effective
        };
    }
}

/// Fills in empty `functionResponse.name`s from the calls of the model turn
/// right before: the k-th response of the turn answers the k-th call.
fn backfill_response_names(contents: &mut [Value]) {
    let mut pending: Vec<String> = Vec::new();
    for content in contents.iter_mut() {
        if role_of(content) == "model" {
            pending = pick(content, &["parts"])
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| pick(part, &FUNCTION_CALL))
                        .map(|call| call_name(call).to_string())
                        .collect()
                })
                .unwrap_or_default();
            continue;
        }
        let names = std::mem::take(&mut pending);
        if names.is_empty() {
            continue;
        }
        let Some(Value::Array(parts)) = content.get_mut("parts") else {
            continue;
        };
        let mut position = 0;
        for part in parts.iter_mut() {
            let Value::Object(part) = part else {
                continue;
            };
            let Some(key) = FUNCTION_RESPONSE
                .into_iter()
                .find(|key| part.get(*key).is_some_and(Value::is_object))
            else {
                continue;
            };
            if let Some(Value::Object(response)) = part.get_mut(key) {
                let unnamed = response
                    .get("name")
                    .and_then(Value::as_str)
                    .is_none_or(|name| name.trim().is_empty());
                if unnamed && let Some(name) = names.get(position).filter(|name| !name.is_empty()) {
                    response.insert("name".to_string(), Value::String(name.clone()));
                }
            }
            position += 1;
        }
    }
}

/// Gemini 3 rejects a replayed model turn whose first `functionCall` part has
/// no thought signature. History a client assembled itself has none, so the
/// documented bypass value is added there. Signed parts are never touched.
fn sign_first_calls(contents: &mut [Value]) {
    for content in contents.iter_mut() {
        if role_of(content) != "model" {
            continue;
        }
        let Some(Value::Array(parts)) = content.get_mut("parts") else {
            continue;
        };
        let first_call = parts
            .iter_mut()
            .find(|part| pick(part, &FUNCTION_CALL).is_some());
        if let Some(part) = first_call
            && raw_signature(part).is_none()
            && let Some(map) = part.as_object_mut()
        {
            map.insert(
                "thoughtSignature".to_string(),
                Value::String(SKIP_SIGNATURE.to_string()),
            );
        }
    }
}

/// The empty user turn put in front of a conversation that starts with a
/// model turn (Gemini requires the first turn to be the user's).
pub(crate) fn empty_user_turn() -> Value {
    json!({"role": "user", "parts": [{"text": ""}]})
}

/// Takes the signatures of other vendors out of the parts of one content.
///
/// A Gemini client is handed foreign signatures armoured as base64 (see
/// [`crate::parts::signature_for_client`]), which the gateway's check for
/// wrapped signatures (`sig::contains_wrapped`) cannot see, so a history
/// carrying one can reach the verbatim path. Gemini rejects a signature it
/// did not issue, so the same policy as in `encode_request` is applied here:
///
/// * a `thought` part signed by another vendor is that vendor's reasoning and
///   is removed, as is a part that only carries such a signature;
/// * any other part (function call, text, media) keeps its payload and loses
///   the signature — `sign_first_calls` then gives a function call the bypass
///   value where one is needed;
/// * a tagged signature that is Gemini's own is restored to its raw form.
///
/// Returns whether the content was left with no parts because of this.
fn scrub_foreign_signatures(content: &mut Value) -> bool {
    let Some(Value::Array(parts)) = content.get_mut("parts") else {
        return false;
    };
    let mut removed = false;
    parts.retain_mut(|part| {
        let Some(raw) = raw_signature(part).filter(|raw| !is_bypass_signature(raw)) else {
            return true;
        };
        let signature = signature_from_client(raw);
        if signature.data == raw {
            // Untagged: Gemini's own, as far as anyone can tell.
            return true;
        }
        let native = signature.valid_for(Protocol::Gemini) && !signature.data.is_empty();
        let Some(map) = part.as_object_mut() else {
            return true;
        };
        remove_signature(map);
        if native {
            map.insert(
                "thoughtSignature".to_string(),
                Value::String(signature.data),
            );
            return true;
        }
        let thought = map.get("thought").and_then(Value::as_bool).unwrap_or(false);
        let empty_text = map
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(str::is_empty);
        let keep = !thought && !empty_text && !is_metadata_only(map);
        removed |= !keep;
        keep
    });
    removed && parts.is_empty()
}

/// Adjusts a native Gemini request that is forwarded verbatim so that the
/// upstream accepts it. Every step is a no-op on a well-formed body:
///
/// * `generationConfig.maxOutputTokens` is lowered to the model's limit when
///   that is known (Gemini rejects larger values);
/// * signatures issued by another vendor are taken out (see
///   [`scrub_foreign_signatures`]);
/// * unknown roles are repaired and empty `functionResponse` names filled in;
/// * the first `functionCall` of each model turn gets the documented
///   signature-validation bypass value if it carries no signature;
/// * a conversation that starts with a model turn gets an empty user turn in
///   front.
///
/// The wrapped form of a `countTokens` request is adjusted inside its
/// `generateContentRequest`.
///
/// Deliberately not done: injecting `safetySettings` (operator policy belongs
/// in payload rules) and rewriting tool declarations.
pub(crate) fn prepare_passthrough(body: &mut Value, ctx: &UpstreamCtx<'_>) {
    let Some(outer) = body.as_object_mut() else {
        return;
    };
    let wrapper = COUNT_WRAPPER
        .into_iter()
        .find(|key| outer.get(*key).is_some_and(Value::is_object));
    let root = match wrapper {
        Some(key) => match outer.get_mut(key) {
            Some(Value::Object(inner)) => inner,
            _ => return,
        },
        None => outer,
    };
    if let Some(limit) = ctx.max_output_tokens.filter(|limit| *limit > 0) {
        for config_key in ["generationConfig", "generation_config"] {
            let Some(Value::Object(config)) = root.get_mut(config_key) else {
                continue;
            };
            for key in ["maxOutputTokens", "max_output_tokens"] {
                if let Some(value) = config.get_mut(key)
                    && num_u64(value).is_some_and(|requested| requested > limit)
                {
                    *value = Value::from(limit);
                }
            }
        }
    }
    let contents = match root.get_mut("contents") {
        Some(Value::Array(contents)) => contents,
        // A single content object: only the signatures can need attention.
        Some(single @ Value::Object(_)) => {
            scrub_foreign_signatures(single);
            return;
        }
        _ => return,
    };
    contents.retain_mut(|content| !scrub_foreign_signatures(content));
    repair_roles(contents);
    backfill_response_names(contents);
    sign_first_calls(contents);
    if contents
        .first()
        .is_some_and(|first| role_of(first) == "model")
    {
        contents.insert(0, empty_user_turn());
    }
}

fn strip_call_ids(content: &mut Value) {
    let Some(Value::Array(parts)) = content.get_mut("parts") else {
        return;
    };
    for part in parts.iter_mut() {
        let Some(part) = part.as_object_mut() else {
            continue;
        };
        for key in FUNCTION_CALL.into_iter().chain(FUNCTION_RESPONSE) {
            if let Some(Value::Object(call)) = part.get_mut(key) {
                call.shift_remove("id");
            }
        }
    }
}

/// Adapts a Gemini API (`generativelanguage`) body for Vertex AI. Apply it to
/// every body sent to a `vertex` provider's Gemini models, after
/// `encode_request` / `encode_count_request` / `prepare_passthrough`.
///
/// Vertex speaks the same `generateContent` schema with two differences that
/// affect bodies:
///
/// * `countTokens` has no `generateContentRequest` wrapper: `contents`,
///   `systemInstruction` and `tools` sit at the top level and the model is
///   named only by the URL. The wrapper is unwrapped here.
/// * `functionCall.id` / `functionResponse.id` are rejected, so they are
///   removed; calls and responses are then paired by position and name, as on
///   every Gemini model before ids existed.
pub fn adapt_for_vertex(body: &mut Value) {
    let Some(root) = body.as_object_mut() else {
        return;
    };
    let wrapped = COUNT_WRAPPER
        .into_iter()
        .find(|key| root.get(*key).is_some_and(Value::is_object))
        .and_then(|key| root.shift_remove(key));
    if let Some(Value::Object(inner)) = wrapped {
        // `contents` next to the wrapper is ignored by the API; the wrapper wins.
        let mut flat = Map::new();
        for (key, value) in inner {
            if key != "model" {
                flat.insert(key, value);
            }
        }
        for (key, value) in std::mem::take(root) {
            flat.entry(key).or_insert(value);
        }
        *root = flat;
    }
    match root.get_mut("contents") {
        Some(Value::Array(contents)) => contents.iter_mut().for_each(strip_call_ids),
        Some(single @ Value::Object(_)) => strip_call_ids(single),
        _ => {}
    }
}
