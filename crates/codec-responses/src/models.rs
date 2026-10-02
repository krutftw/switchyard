//! Model listings in the OpenAI shape (`GET /v1/models`).

use serde_json::{Value, json};
use switchyard_core::ModelInfo;

/// Owner reported for models whose vendor is unknown: the gateway itself
/// serves them.
const DEFAULT_OWNER: &str = "switchyard";

/// One entry: exactly the four fields of OpenAI's `Model` object. `created`
/// and `owned_by` are mandatory there, so unknown values are reported as `0`
/// and the gateway's own name rather than left out.
pub(crate) fn encode_model(model: &ModelInfo) -> Value {
    json!({
        "id": model.id,
        "object": "model",
        "created": model.created.unwrap_or(0),
        "owned_by": model.owned_by.as_deref().unwrap_or(DEFAULT_OWNER),
    })
}

/// `{"object":"list","data":[…]}` in the order given.
pub(crate) fn encode_models(models: &[ModelInfo]) -> Value {
    json!({
        "object": "list",
        "data": models.iter().map(encode_model).collect::<Vec<_>>(),
    })
}
