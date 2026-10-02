//! Structural validators for the four vendors' request, response and stream
//! formats.
//!
//! They are written from the vendors' documented rules (reference notes
//! `15-api-research.md`, plus the pairwise notes `06`–`09` where those state a
//! vendor constraint) and deliberately share no code with the codecs: a
//! validator that used a codec's own helpers would agree with the codec's
//! bugs. Each validator collects every violation it finds and returns them
//! all, so a failing matrix cell shows the whole picture at once.
//!
//! The validators are *strict*: they model the real vendor API, not the
//! lenient servers that imitate it. Where a vendor is known to tolerate
//! something the gateway never needs to send, the validator rejects it.

use serde_json::{Map, Value};
use switchyard_core::SseEvent;

mod anthropic;
mod chat;
mod gemini;
mod responses;

pub use anthropic::{
    validate_anthropic_request, validate_anthropic_response, validate_anthropic_stream,
};
pub use chat::{
    validate_chat_compatible_request, validate_chat_request, validate_chat_response,
    validate_chat_stream,
};
pub use gemini::{validate_gemini_request, validate_gemini_response, validate_gemini_stream};
pub use responses::{
    validate_responses_request, validate_responses_response, validate_responses_stream,
};

/// `Ok(())` or every violation found.
pub type Report = Result<(), Vec<String>>;

/// Collects violations, each prefixed with the JSON path it was found at.
#[derive(Default)]
pub(crate) struct Errs(Vec<String>);

impl Errs {
    pub(crate) fn push(&mut self, path: &str, message: impl AsRef<str>) {
        self.0.push(format!("{path}: {}", message.as_ref()));
    }

    pub(crate) fn finish(self) -> Report {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(self.0)
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The value as an object, reporting when it is not one.
pub(crate) fn object<'a>(
    value: &'a Value,
    path: &str,
    errs: &mut Errs,
) -> Option<&'a Map<String, Value>> {
    match value.as_object() {
        Some(map) => Some(map),
        None => {
            errs.push(path, format!("expected an object, found {}", kind(value)));
            None
        }
    }
}

/// JSON type name of a value, for messages.
pub(crate) fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Reports keys of `map` that are not in `allowed`.
pub(crate) fn only_keys(map: &Map<String, Value>, allowed: &[&str], path: &str, errs: &mut Errs) {
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            errs.push(path, format!("unknown field `{key}`"));
        }
    }
}

/// A required string field. Returns it when present and a string.
pub(crate) fn req_str<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    path: &str,
    errs: &mut Errs,
) -> Option<&'a str> {
    match map.get(key) {
        Some(Value::String(text)) => Some(text.as_str()),
        Some(other) => {
            errs.push(
                path,
                format!("`{key}` must be a string, found {}", kind(other)),
            );
            None
        }
        None => {
            errs.push(path, format!("missing required field `{key}`"));
            None
        }
    }
}

/// A required, non-empty string field.
pub(crate) fn req_nonempty<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    path: &str,
    errs: &mut Errs,
) -> Option<&'a str> {
    let text = req_str(map, key, path, errs)?;
    if text.is_empty() {
        errs.push(path, format!("`{key}` must not be empty"));
        return None;
    }
    Some(text)
}

/// An optional field that must be a string when present (and not null).
pub(crate) fn opt_str<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    path: &str,
    errs: &mut Errs,
) -> Option<&'a str> {
    match map.get(key) {
        None => None,
        Some(Value::String(text)) => Some(text.as_str()),
        Some(other) => {
            errs.push(
                path,
                format!("`{key}` must be a string, found {}", kind(other)),
            );
            None
        }
    }
}

/// An optional boolean field.
pub(crate) fn opt_bool(
    map: &Map<String, Value>,
    key: &str,
    path: &str,
    errs: &mut Errs,
) -> Option<bool> {
    match map.get(key) {
        None => None,
        Some(Value::Bool(flag)) => Some(*flag),
        Some(other) => {
            errs.push(
                path,
                format!("`{key}` must be a boolean, found {}", kind(other)),
            );
            None
        }
    }
}

/// An optional non-negative integer field (a JSON integer, not a float).
pub(crate) fn opt_uint(
    map: &Map<String, Value>,
    key: &str,
    path: &str,
    errs: &mut Errs,
) -> Option<u64> {
    match map.get(key) {
        None => None,
        Some(value) => match value.as_u64() {
            Some(n) => Some(n),
            None => {
                errs.push(
                    path,
                    format!("`{key}` must be a non-negative integer, found {value}"),
                );
                None
            }
        },
    }
}

/// A required non-negative integer field.
pub(crate) fn req_uint(
    map: &Map<String, Value>,
    key: &str,
    path: &str,
    errs: &mut Errs,
) -> Option<u64> {
    if !map.contains_key(key) {
        errs.push(path, format!("missing required field `{key}`"));
        return None;
    }
    opt_uint(map, key, path, errs)
}

/// An optional number within `[low, high]`.
pub(crate) fn opt_number_in(
    map: &Map<String, Value>,
    key: &str,
    low: f64,
    high: f64,
    path: &str,
    errs: &mut Errs,
) -> Option<f64> {
    match map.get(key) {
        None => None,
        Some(value) => match value.as_f64() {
            Some(n) if n >= low && n <= high => Some(n),
            Some(n) => {
                errs.push(path, format!("`{key}` = {n} is outside [{low}, {high}]"));
                None
            }
            None => {
                errs.push(
                    path,
                    format!("`{key}` must be a number, found {}", kind(value)),
                );
                None
            }
        },
    }
}

/// `^[a-zA-Z0-9_-]{1,max}$`: the identifier shape OpenAI and Anthropic use
/// for tool names (and Anthropic for tool-use ids).
pub(crate) fn is_ident(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether `text` is one JSON document that is an object.
pub(crate) fn is_json_object_text(text: &str) -> bool {
    matches!(serde_json::from_str::<Value>(text), Ok(Value::Object(_)))
}

/// Whether `text` is plausible standard base64 (padded or not).
pub(crate) fn is_base64(text: &str) -> bool {
    let body = text.trim_end_matches('=');
    !body.is_empty()
        && text.len() - body.len() <= 2
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// Whether a blob carries the gateway's own cross-vendor wrapping, either in
/// the clear (`sy1.<tag>.…`) or base64-armoured (the base64 of `sy1.` always
/// starts with `c3kxL`). Such a string must never reach an upstream: no
/// vendor issued it.
pub(crate) fn is_wrapped_blob(text: &str) -> bool {
    text.starts_with("sy1.") || text.starts_with("c3kxL")
}

/// Parses the `data` of an SSE event as JSON, reporting a failure.
pub(crate) fn event_json(event: &SseEvent, index: usize, errs: &mut Errs) -> Option<Value> {
    match serde_json::from_str::<Value>(&event.data) {
        Ok(value) => Some(value),
        Err(error) => {
            errs.push(
                &format!("event[{index}]"),
                format!("data is not JSON ({error}): {:.80}", event.data),
            );
            None
        }
    }
}

/// A `data:` URI with a base64 payload: `data:<media-type>;base64,<data>`.
/// Returns the media type.
pub(crate) fn data_uri_media_type(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let (media_type, encoding) = meta.split_once(';')?;
    (encoding == "base64" && !media_type.is_empty() && !data.is_empty()).then_some(media_type)
}

/// An `http(s)` URL or a base64 `data:` URI.
pub(crate) fn is_url_or_data_uri(text: &str) -> bool {
    text.starts_with("http://")
        || text.starts_with("https://")
        || data_uri_media_type(text).is_some()
}
