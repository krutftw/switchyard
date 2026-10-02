//! Model listings in the OpenAI shape (`GET /v1/models`, `GET /v1/models/{id}`).

use serde_json::{Value, json};
use switchyard_core::ModelInfo;

/// `owned_by` for models whose owner the gateway does not know. The field is
/// a required string in OpenAI's schema.
const DEFAULT_OWNER: &str = "switchyard";

/// One model object: `{"id","object":"model","created","owned_by"}`.
///
/// Only these four fields exist in OpenAI's schema, so richer metadata
/// (context window, reasoning support) is not exposed here. `created` is `0`
/// when the release time is unknown: typed clients require an integer.
pub(crate) fn encode_model(model: &ModelInfo) -> Value {
    json!({
        "id": model.id,
        "object": "model",
        "created": model.created.unwrap_or(0),
        "owned_by": model.owned_by.as_deref().unwrap_or(DEFAULT_OWNER),
    })
}

/// The list envelope: `{"object":"list","data":[…]}`, in the given order.
pub(crate) fn encode_models(models: &[ModelInfo]) -> Value {
    json!({
        "object": "list",
        "data": models.iter().map(encode_model).collect::<Vec<_>>(),
    })
}
