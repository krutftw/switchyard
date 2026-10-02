//! Review findings in service-account token minting.
//!
//! These started as failing tests left by the adversarial review (findings
//! UP-9, UP-10) and are kept as regression tests: the doc comment of each test
//! describes the defect as it was found, the assertions the behaviour that
//! is now implemented.

mod common;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use common::serve;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use switchyard_core::config::ProxySetting;
use switchyard_upstream::{HttpClients, ServiceAccount, TokenSource};

const TEST_KEY_PEM: &str = include_str!("fixtures/test_rsa_pkcs8.pem");

fn account(token_uri: &str) -> Arc<ServiceAccount> {
    let key = json!({
        "type": "service_account",
        "project_id": "demo-project",
        "private_key_id": "0123456789abcdef",
        "private_key": TEST_KEY_PEM,
        "client_email": "gateway@demo-project.iam.gserviceaccount.com",
        "token_uri": token_uri,
    });
    Arc::new(ServiceAccount::from_json(&key.to_string()).unwrap())
}

fn http() -> reqwest::Client {
    HttpClients::new()
        .unwrap()
        .client(&ProxySetting::Direct, Duration::from_secs(5))
        .unwrap()
}

async fn absurd_lifetime() -> Response {
    // A broken (or hostile) token endpoint: `expires_in` far beyond anything
    // a clock can represent. The key file's `token_uri` decides who answers.
    axum::Json(json!({
        "access_token": "ya29.review-token",
        "token_type": "Bearer",
        "expires_in": 1e30
    }))
    .into_response()
}

/// DESIGN "Rules for everyone": no panics on data that comes from the
/// network. `TokenSource` computes `Instant::now() + (expires_in - 60 s)`
/// with the `expires_in` the token endpoint sent; a huge value overflows the
/// addition and panics ("overflow when adding duration to instant") inside
/// the request task. The token is perfectly usable: it should be returned
/// (with the lifetime clamped) — or the answer rejected with an error — but
/// the call must not crash.
#[tokio::test]
async fn an_absurd_expires_in_does_not_panic() {
    let addr = serve(Router::new().route("/token", post(absurd_lifetime))).await;
    let source = TokenSource::new(account(&format!("http://{addr}/token")));
    let http = http();
    let outcome =
        tokio::spawn(async move { source.token(&http).await.map_err(|e| e.info.message) })
            .await
            .expect("TokenSource::token panicked on the token endpoint's answer");
    if let Ok(token) = outcome {
        assert_eq!(token, "ya29.review-token");
    }
}

async fn failing(State(exchanges): State<Arc<AtomicUsize>>) -> Response {
    exchanges.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(250)).await;
    (StatusCode::SERVICE_UNAVAILABLE, "token service unavailable").into_response()
}

/// Track requirement: token minting is "single-flight under concurrency".
///
/// That holds only when the exchange succeeds. When it fails, every caller
/// that was waiting for the in-flight exchange starts its *own* exchange,
/// one after the other (the lock is held across each): N concurrent Vertex
/// requests produce N exchanges and the last caller waits N times the
/// exchange time — up to N x 30 s (`EXCHANGE_TIMEOUT`) against a token
/// endpoint that hangs, none of it bounded by the caller's request timeout.
/// Callers that waited for a flight must share its outcome.
#[tokio::test]
async fn a_failed_exchange_is_shared_by_the_callers_that_waited_for_it() {
    let exchanges = Arc::new(AtomicUsize::new(0));
    let addr = serve(
        Router::new()
            .route("/token", post(failing))
            .with_state(exchanges.clone()),
    )
    .await;
    let source = Arc::new(TokenSource::new(account(&format!("http://{addr}/token"))));
    let http = http();

    let started = Instant::now();
    let callers: Vec<_> = (0..8)
        .map(|_| {
            let source = source.clone();
            let http = http.clone();
            tokio::spawn(async move { source.token(&http).await })
        })
        .collect();
    for caller in callers {
        assert!(caller.await.unwrap().is_err());
    }
    let elapsed = started.elapsed();

    assert_eq!(
        exchanges.load(Ordering::SeqCst),
        1,
        "8 concurrent callers performed {} token exchanges (took {elapsed:?})",
        exchanges.load(Ordering::SeqCst)
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "callers queued behind each other's failed exchanges: {elapsed:?}"
    );
}
