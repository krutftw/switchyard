//! Model listings (`GET /v1beta/models`, `GET /v1beta/models/{name}`) and the
//! `countTokens` result, in Gemini's shape.

use crate::util::{num_u64, pick, strip_models_prefix};
use serde_json::{Map, Value, json};
use switchyard_core::ModelInfo;

/// The generation methods every model served through the gateway supports.
const METHODS: [&str; 3] = ["generateContent", "countTokens", "streamGenerateContent"];

/// One `Model` resource. `displayName` and `description` fall back to the
/// model id so clients that show them never get an empty label; the token
/// limits and the `thinking` flag appear only when the gateway knows them.
pub(crate) fn encode_model(model: &ModelInfo) -> Value {
    let id = strip_models_prefix(&model.id);
    let mut out = Map::new();
    out.insert("name".to_string(), Value::String(format!("models/{id}")));
    out.insert("version".to_string(), Value::from("001"));
    out.insert(
        "displayName".to_string(),
        Value::from(
            model
                .display_name
                .as_deref()
                .filter(|n| !n.is_empty())
                .unwrap_or(id),
        ),
    );
    out.insert(
        "description".to_string(),
        Value::from(
            model
                .description
                .as_deref()
                .filter(|d| !d.is_empty())
                .unwrap_or(id),
        ),
    );
    if let Some(limit) = model.context_window {
        out.insert("inputTokenLimit".to_string(), Value::from(limit));
    }
    if let Some(limit) = model.max_output_tokens {
        out.insert("outputTokenLimit".to_string(), Value::from(limit));
    }
    out.insert("supportedGenerationMethods".to_string(), json!(METHODS));
    if model.thinking.is_some() {
        out.insert("thinking".to_string(), Value::Bool(true));
    } else if model.known {
        out.insert("thinking".to_string(), Value::Bool(false));
    }
    Value::Object(out)
}

/// The `models.list` response. Everything fits on one page, so there is no
/// `nextPageToken`.
pub(crate) fn encode_models(models: &[ModelInfo]) -> Value {
    json!({"models": models.iter().map(encode_model).collect::<Vec<_>>()})
}

/// The `countTokens` response.
pub(crate) fn encode_count_response(input_tokens: u64) -> Value {
    json!({"totalTokens": input_tokens})
}

/// Reads `totalTokens` from a `countTokens` response.
pub(crate) fn decode_count_response(body: &Value) -> Option<u64> {
    pick(body, &["totalTokens", "total_tokens"]).and_then(num_u64)
}
