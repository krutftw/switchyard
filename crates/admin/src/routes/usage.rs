//! Usage statistics, the request list and the application log. The
//! telemetry crate's result types are returned verbatim; its query types
//! read query strings leniently (an unknown value falls back to the default
//! instead of failing the request).
//!
//! The usage store answers from memory under a lock and reads files for
//! old records and captured bodies, so every call runs off the async
//! threads.

use super::{Params, PathParam};
use crate::Shared;
use crate::error::{ApiFailure, ApiResult, ok_json};
use crate::state::blocking;
use axum::extract::State;
use serde_json::json;
use switchyard_core::util::now_unix_ms;
use switchyard_telemetry::{LogQuery, RequestQuery, UsageQuery};

/// `GET /usage/summary?range=`.
pub(crate) async fn summary(
    State(state): State<Shared>,
    Params(query): Params<UsageQuery>,
) -> ApiResult {
    let telemetry = state.gateway.telemetry().clone();
    let summary = blocking(move || telemetry.usage().summary(query.range(), now_unix_ms())).await?;
    ok_json(&summary)
}

/// `GET /usage/timeseries?range=&bucket=&group_by=`.
pub(crate) async fn timeseries(
    State(state): State<Shared>,
    Params(query): Params<UsageQuery>,
) -> ApiResult {
    let telemetry = state.gateway.telemetry().clone();
    let series = blocking(move || {
        telemetry.usage().timeseries(
            query.range(),
            query.bucket(),
            query.group_by(),
            now_unix_ms(),
        )
    })
    .await?;
    ok_json(&series)
}

/// `GET /requests?limit=&before=&model=&provider=&key=&status=&q=`.
pub(crate) async fn requests(
    State(state): State<Shared>,
    Params(query): Params<RequestQuery>,
) -> ApiResult {
    let telemetry = state.gateway.telemetry().clone();
    let page = blocking(move || telemetry.usage().requests(&query)).await?;
    ok_json(&page)
}

/// `GET /requests/{id}`: the record and, when bodies were captured for it,
/// the bodies (already redacted and truncated by the telemetry crate).
pub(crate) async fn request(State(state): State<Shared>, PathParam(id): PathParam) -> ApiResult {
    let telemetry = state.gateway.telemetry().clone();
    let lookup = id.clone();
    let (record, bodies) = blocking(move || {
        let record = telemetry.usage().find(&lookup);
        let bodies = record
            .as_ref()
            .and_then(|_| telemetry.bodies().read(&lookup));
        (record, bodies)
    })
    .await?;
    let record =
        record.ok_or_else(|| ApiFailure::not_found(format!("there is no request `{id}`")))?;
    ok_json(&json!({
        "record": record,
        "bodies": bodies,
    }))
}

/// `DELETE /usage`: forgets every statistic, the request list and the usage
/// files. Totals since start (`GET /status`) and captured bodies stay.
pub(crate) async fn clear(State(state): State<Shared>) -> ApiResult {
    let cleared = std::sync::Arc::clone(&state);
    blocking(move || {
        cleared.gateway.telemetry().usage().clear();
        // The usage files are gone, and with them what was read from them.
        cleared.key_usage.reset();
    })
    .await?;
    tracing::info!("usage statistics cleared through the admin API");
    ok_json(&json!({ "ok": true }))
}

/// `GET /logs?limit=&level=&q=&target=&before=`: a page of the log buffer,
/// plus `started_at`, the start of this process. Sequence numbers begin at
/// 1 again after a restart, so a client that combines pages with live
/// events needs to know which process the numbers belong to.
pub(crate) async fn logs(
    State(state): State<Shared>,
    Params(query): Params<LogQuery>,
) -> ApiResult {
    let telemetry = state.gateway.telemetry();
    let mut page = crate::views::to_value(&telemetry.logs().page(&query));
    if let serde_json::Value::Object(fields) = &mut page {
        fields.insert(
            "started_at".to_string(),
            json!(telemetry.gauges().started_at()),
        );
    }
    ok_json(&page)
}
