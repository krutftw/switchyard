//! Model listings in the Messages API shape (`GET /v1/models`).

use crate::util::rfc3339;
use serde_json::{Value, json};
use switchyard_core::ModelInfo;

/// One `ModelInfo` object. Limits the gateway does not know are `null`, as
/// the vendor does for models without published limits.
pub(crate) fn encode_model(model: &ModelInfo) -> Value {
    json!({
        "type": "model",
        "id": model.id,
        "display_name": model.display_name.as_deref().unwrap_or(&model.id),
        "created_at": rfc3339(model.created.unwrap_or(0)),
        "max_input_tokens": model.context_window,
        "max_tokens": model.max_output_tokens,
    })
}

/// The list envelope. The gateway always returns the whole list, in the
/// order given, so `has_more` is false and the cursor ids are simply the
/// first and last entries (`null` for an empty list).
pub(crate) fn encode_models(models: &[ModelInfo]) -> Value {
    json!({
        "data": models.iter().map(encode_model).collect::<Vec<_>>(),
        "has_more": false,
        "first_id": models.first().map(|model| model.id.as_str()),
        "last_id": models.last().map(|model| model.id.as_str()),
    })
}
