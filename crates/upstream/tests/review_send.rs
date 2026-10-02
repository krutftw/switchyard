//! Review findings in `UpstreamClient::send`.
//!
//! These started as failing tests left by the adversarial review (findings
//! UP-6, UP-9) and are kept as regression tests: the doc comment of each test
//! describes the defect as it was found, the assertions the behaviour that
//! is now implemented.

mod common;

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use common::{client, openai, serve};
use serde_json::json;
use std::time::Duration;
use switchyard_core::FailureClass;
use switchyard_core::config::UpstreamConfig;
use switchyard_upstream::{Operation, Timeouts};

/// A compatible server that quotes the credential it rejected, as naive
/// implementations do ("Invalid API key: <key>").
async fn echoes_the_key(headers: HeaderMap) -> Response {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim_start_matches("Bearer ")
        .to_string();
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        json!({"error": {
            "message": format!("Invalid API key: {presented}"),
            "type": "invalid_request_error",
            "code": "invalid_api_key"
        }})
        .to_string(),
    )
        .into_response()
}

/// DESIGN "Rules for everyone": "Never log or return API keys".
///
/// `send()` knows the credential it used, but the `UpstreamError` it
/// returns carries the upstream's body and message verbatim. When an
/// upstream quotes the key in its rejection, the gateway's own upstream
/// credential ends up in `info.message` (which `to_api_error` turns into the
/// client-visible message, and `classify` writes to the debug log) and in
/// `body` (which the gateway forwards as is on same-protocol failures and
/// stores in request logs). The scheduler crate scrubs the secret from its
/// own copy of the message; the error handed to the gateway is not scrubbed.
#[tokio::test]
async fn an_upstream_that_echoes_the_credential_does_not_leak_it() {
    let addr = serve(Router::new().route("/v1/chat/completions", post(echoes_the_key))).await;
    let error = client()
        .send(
            &openai(addr),
            &Operation::Generate { stream: false },
            Bytes::from_static(b"{}"),
            &HeaderMap::new(),
            common::timeouts(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 401);
    assert_eq!(error.class, FailureClass::Auth);
    assert!(
        !error.info.message.contains(common::KEY),
        "the upstream credential is in the error message: {}",
        error.info.message
    );
    assert!(
        !error.body.as_deref().unwrap_or("").contains(common::KEY),
        "the upstream credential is in the kept error body: {:?}",
        error.body
    );
    assert!(
        !error.to_api_error().message.contains(common::KEY),
        "the upstream credential would be shown to the client"
    );
}

async fn overloaded() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "application/json")],
        json!({"error": {"message": "The server is overloaded.", "type": "service_unavailable_error", "code": "server_is_overloaded"}})
            .to_string(),
    )
        .into_response()
}

/// DESIGN "Rules for everyone": no panics on data that comes from the
/// config file. `[upstream] request_timeout_secs` is an unvalidated `u64`
/// (core `Config::validate` puts no upper bound on it) and
/// `Timeouts::from_config` turns it into a `Duration` as is. A successful
/// call copes with any value, but as soon as the upstream answers with a
/// non-2xx status, `read_capped` computes `Instant::now() + timeouts.request`
/// and panics ("overflow when adding duration to instant") instead of
/// returning the classified error.
#[tokio::test]
async fn a_huge_request_timeout_does_not_panic_on_error_responses() {
    let addr = serve(Router::new().route("/v1/chat/completions", post(overloaded))).await;

    let config = UpstreamConfig {
        request_timeout_secs: u64::MAX,
        ..UpstreamConfig::default()
    };
    let timeouts = Timeouts::from_config(&config);
    assert_eq!(timeouts.request, Duration::from_secs(u64::MAX));

    let target = openai(addr);
    let outcome = tokio::spawn(async move {
        client()
            .send(
                &target,
                &Operation::Generate { stream: false },
                Bytes::from_static(b"{}"),
                &HeaderMap::new(),
                timeouts,
            )
            .await
            .map(|_| ())
    })
    .await;
    let outcome = outcome.expect("send() panicked while reading the error response");
    let error = outcome.unwrap_err();
    assert_eq!(error.status, 503);
    assert_eq!(error.class, FailureClass::Server);
}
