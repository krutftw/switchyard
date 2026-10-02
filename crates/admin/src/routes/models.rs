//! The model table, the built-in catalog, and the three whole-section
//! editors: aliases, payload rules, prices.

use super::JsonBody;
use crate::Shared;
use crate::error::{ApiResult, ok_json};
use crate::views::{aliases_view, payload_view, pricing_view};
use axum::extract::State;
use switchyard_core::config::{AliasConfig, PayloadConfig, PriceConfig};

/// `GET /models`: every client-facing model name with its routes and how
/// many credentials could serve it right now.
pub(crate) async fn models(State(state): State<Shared>) -> ApiResult {
    ok_json(&state.gateway.scheduler().models())
}

/// `GET /catalog`: the built-in model metadata.
pub(crate) async fn catalog() -> ApiResult {
    ok_json(&switchyard_scheduler::catalog().entries())
}

/// `GET /aliases`.
pub(crate) async fn get_aliases(State(state): State<Shared>) -> ApiResult {
    ok_json(&aliases_view(&state.gateway.config().aliases))
}

/// `PUT /aliases`: replaces the whole list.
pub(crate) async fn put_aliases(
    State(state): State<Shared>,
    JsonBody(aliases): JsonBody<Vec<AliasConfig>>,
) -> ApiResult {
    let (config, ()) = state
        .edit_config(move |config| {
            config.aliases = aliases;
            Ok(())
        })
        .await?;
    ok_json(&aliases_view(&config.aliases))
}

/// `GET /payload`.
pub(crate) async fn get_payload(State(state): State<Shared>) -> ApiResult {
    ok_json(&payload_view(&state.gateway.config().payload))
}

/// `PUT /payload`: replaces all three rule lists.
pub(crate) async fn put_payload(
    State(state): State<Shared>,
    JsonBody(payload): JsonBody<PayloadConfig>,
) -> ApiResult {
    let (config, ()) = state
        .edit_config(move |config| {
            config.payload = payload;
            Ok(())
        })
        .await?;
    ok_json(&payload_view(&config.payload))
}

/// `GET /pricing`.
pub(crate) async fn get_pricing(State(state): State<Shared>) -> ApiResult {
    ok_json(&pricing_view(&state.gateway.config().pricing))
}

/// `PUT /pricing`: replaces the whole list.
pub(crate) async fn put_pricing(
    State(state): State<Shared>,
    JsonBody(pricing): JsonBody<Vec<PriceConfig>>,
) -> ApiResult {
    let (config, ()) = state
        .edit_config(move |config| {
            config.pricing = pricing;
            Ok(())
        })
        .await?;
    ok_json(&pricing_view(&config.pricing))
}
