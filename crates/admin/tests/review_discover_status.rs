//! Regression tests for review finding ADM-R2 (fixed): `POST
//! /providers/{name}/discover` answered with the *upstream's* status class
//! as if it were the admin API's own.
//!
//! The handler used to convert the gateway's `ApiError` straight into the
//! admin envelope, status included. For the admin API these statuses
//! already mean something else (API.md, "Errors"):
//!
//! * `429` + `Retry-After` — "locked out after repeated wrong secrets". An
//!   upstream that rate-limits its model listing makes the admin API answer
//!   exactly that, `Retry-After` and all.
//! * `404` — "no such route, provider, credential, key or request". An
//!   upstream without a `/models` endpoint (many OpenAI-compatible servers)
//!   makes an *existing* provider answer 404.
//! * `422` — "the configuration it would produce is not valid. Always with
//!   `issues`". An upstream 422 comes back as a 422 without issues.
//! * `400` — "the request is malformed", although the admin request was fine.
//!
//! Now: a failed upstream listing is a 5xx of the admin API (502, or 504
//! for a timeout) carrying the upstream's explanation in the message —
//! never one of the statuses the admin API uses for its own conditions, and
//! without a `Retry-After` header. The admin API's own two conditions keep
//! their statuses: 404 for an unknown provider, 503 for a provider without
//! a usable credential.

mod support;

use axum::extract::Path;
use http::{Method, StatusCode};
use serde_json::json;
use support::{App, BASE, read_with_headers};

/// An upstream whose model listing answers with the status in its path:
/// `http://addr/<status>/v1/models`.
async fn failing_upstream() -> std::net::SocketAddr {
    use axum::response::IntoResponse;
    use axum::routing::get;
    let app = axum::Router::new().route(
        "/{status}/v1/models",
        get(|Path(status): Path<u16>| async move {
            let mut response = (
                StatusCode::from_u16(status).unwrap(),
                axum::Json(json!({"error": {
                    "message": format!("upstream says {status}"),
                    "type": "some_error",
                }})),
            )
                .into_response();
            if status == 429 {
                response
                    .headers_mut()
                    .insert("retry-after", "17".parse().unwrap());
            }
            response
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

async fn app_with(statuses: &[u16]) -> App {
    let upstream = failing_upstream().await;
    let mut config = BASE.to_string();
    for status in statuses {
        config.push_str(&format!(
            r#"
[[providers]]
name = "up{status}"
kind = "openai-compat"
base_url = "http://{upstream}/{status}/v1"
discover = false
api_keys = ["sk-review-key-{status}-aaaaaaaaaaaaaaaa"]
[[providers.models]]
id = "m{status}"
"#
        ));
    }
    App::start_config(&config).await
}

async fn discover(app: &App, status: u16) -> (StatusCode, http::HeaderMap, serde_json::Value) {
    read_with_headers(
        app.request(Method::POST, &format!("/providers/up{status}/discover"))
            .json(&json!({})),
    )
    .await
}

#[tokio::test]
async fn a_rate_limited_upstream_listing_is_not_the_admin_lockout() {
    let app = app_with(&[429]).await;
    let (status, headers, body) = discover(&app, 429).await;
    // 429 with Retry-After is what a locked-out address gets from every
    // admin route; the operator here presented the right secret.
    assert_ne!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "discover reported the upstream's rate limit as the admin lockout: {body}"
    );
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("upstream says 429"),
        "{body}"
    );
    let _ = headers;
}

#[tokio::test]
async fn an_upstream_without_a_model_listing_is_not_an_unknown_provider() {
    let app = app_with(&[404]).await;
    // The provider exists.
    assert_eq!(app.get("/providers/up404").await.0, StatusCode::OK);
    let (status, _, body) = discover(&app, 404).await;
    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "discover answered 404 for a provider that exists: {body}"
    );
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
}

#[tokio::test]
async fn upstream_request_faults_are_not_admin_validation_errors() {
    let app = app_with(&[400, 422]).await;
    for upstream_status in [400, 422] {
        let (status, _, body) = discover(&app, upstream_status).await;
        assert!(
            status.is_server_error(),
            "upstream {upstream_status} on the model listing became admin {status}: {body}"
        );
    }
}

#[tokio::test]
async fn every_upstream_failure_is_a_502_that_says_what_the_upstream_said() {
    let statuses = [400, 401, 402, 403, 404, 409, 422, 429, 500, 502, 503, 529];
    let app = app_with(&statuses).await;
    for upstream_status in statuses {
        let (status, headers, body) = discover(&app, upstream_status).await;
        assert_eq!(
            status,
            StatusCode::BAD_GATEWAY,
            "upstream {upstream_status}: {body}"
        );
        // The admin error shape, with the upstream's explanation and
        // without the lockout's header or a validation's issues.
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains(&format!("upstream says {upstream_status}")),
            "upstream {upstream_status}: {body}"
        );
        assert!(
            message.starts_with("the upstream "),
            "upstream {upstream_status}: {body}"
        );
        assert!(body["error"].get("issues").is_none(), "{body}");
        assert!(
            !headers.contains_key("retry-after"),
            "upstream {upstream_status}: a Retry-After header on a discover failure"
        );
        // The credential the upstream was asked with is not quoted back.
        assert!(!body.to_string().contains("sk-review-key"), "{body}");
    }

    // An upstream that gave up waiting is the one case with its own
    // status: the gateway's timeout class, 504.
    let timed_out = app_with(&[408]).await;
    let (status, headers, body) = discover(&timed_out, 408).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("the upstream did not answer the model listing request in time"),
        "{body}"
    );
    assert!(!headers.contains_key("retry-after"));

    // The wait a rate-limiting upstream asked for is in the sentence.
    let (_, _, body) = discover(&app, 429).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("it asks to wait 17 seconds"),
        "{body}"
    );
}

#[tokio::test]
async fn the_admin_apis_own_discover_failures_keep_their_statuses() {
    let mut config = BASE.to_string();
    config.push_str(
        r#"
[[providers]]
name = "keyless"
kind = "openai"
discover = false
api_keys = ["env:SWITCHYARD_ADMIN_TEST_VARIABLE_THAT_IS_NOT_SET"]
[[providers.models]]
id = "gpt-test"
"#,
    );
    let app = App::start_config(&config).await;

    // No such provider: 404, as for every other provider route.
    let (status, body) = app.post("/providers/nope/discover", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body["error"]["message"],
        "there is no provider named `nope`"
    );

    // Nothing to ask the upstream with: 503, and only then.
    let (status, body) = app.post("/providers/keyless/discover", json!({})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        body["error"]["message"],
        "provider `keyless` has no usable credential to ask its upstream with"
    );

    // A sign-in that is still fine afterwards: none of this was a lockout.
    assert_eq!(app.get("/status").await.0, StatusCode::OK);
}
