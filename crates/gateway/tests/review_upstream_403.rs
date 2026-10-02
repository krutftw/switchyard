//! Review finding GW-10: an upstream `403` is never handed to the client as
//! it is, whatever the transport classified it as.
//!
//! The track's requirement: "upstream 401/403 credential failures are never
//! forwarded as-is … — the client's own key is fine"; DESIGN.md section 8,
//! step 6: "Upstream auth failures are reported as 502 (the client's key is
//! fine)".
//!
//! `Failover::into_error` only withholds the upstream's body and status when
//! the failure's *class* is `Auth`. But `switchyard_upstream::classify` puts
//! the most common real-world 403s into other classes, because of what the
//! scheduler should do about them (rest the model, not the whole key):
//!
//! * OpenAI: `403 {"error":{"message":"Project `proj_…` does not have access
//!   to model `gpt-5`","code":"model_not_found"}}` → `ModelNotFound`;
//! * Vertex AI: `403 PERMISSION_DENIED … denied on resource
//!   '//aiplatform.googleapis.com/projects/<project>/…/models/<model>' (or
//!   it may not exist)` → `ModelNotFound`.
//!
//! Both say "the *gateway's* credential may not use this". On the passthrough
//! path they reach the client verbatim with status 403: the client's SDK
//! raises a permission error about a key that is perfectly fine, and the body
//! names the gateway operator's upstream project. (The translation path
//! already answers such a failure with its own 404.)

// The shared support module re-exports more than one review file uses.
#[allow(unused_imports)]
mod support;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;
use support::{Harness, Output};
use switchyard_core::Protocol;

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn assert_not_forwarded(what: &str, output: &Output, secret: &str) {
    let text = String::from_utf8_lossy(&output.body).to_string();
    assert!(
        output.status != 403 && output.status != 401,
        "{what}: the upstream's {} was forwarded as the client's own status: {text}",
        output.status
    );
    assert!(
        output.status >= 400,
        "{what}: the request failed upstream: {}",
        output.status
    );
    assert!(
        !text.contains(secret),
        "{what}: the upstream's rejection of the gateway's credential is quoted verbatim: {text}"
    );
}

/// OpenAI's answer for a model the key's project has not been given.
#[tokio::test]
async fn an_openai_403_about_the_gateways_project_is_not_the_clients_403() {
    let app = Router::new().fallback(|| async {
        (
            StatusCode::FORBIDDEN,
            [("content-type", "application/json")],
            json!({"error": {
                "message": "Project `proj_GATEWAYsOwnProject` does not have access to model `gpt-5`",
                "type": "invalid_request_error",
                "param": null,
                "code": "model_not_found"
            }})
            .to_string(),
        )
            .into_response()
    });
    let base = serve(app).await;
    let config = format!(
        r#"
[[providers]]
name = "oai"
kind = "openai"
base_url = "{base}/v1"
api_keys = ["sk-gateway-key-0123456789"]
[[providers.models]]
id = "gpt-5"
"#
    );
    for stream in [false, true] {
        // A fresh gateway each time: this is about the request that runs
        // into the refusal, not about the ones that find the model resting.
        let harness = Harness::start(&config).await;
        let output = harness.ask(Protocol::OpenaiChat, "gpt-5", stream).await;
        assert_not_forwarded(
            &format!("chat passthrough (stream={stream})"),
            &output,
            "proj_GATEWAYsOwnProject",
        );
    }
}

/// Vertex AI's answer when the gateway's service account (or key) lacks the
/// IAM permission to call the model.
#[tokio::test]
async fn a_google_permission_denied_is_not_the_clients_403() {
    let app = Router::new().fallback(|| async {
        (
            StatusCode::FORBIDDEN,
            [("content-type", "application/json")],
            json!({"error": {
                "code": 403,
                "message": "Permission 'aiplatform.endpoints.predict' denied on resource '//aiplatform.googleapis.com/projects/gateway-owners-project/locations/us-central1/publishers/google/models/gemini-up' (or it may not exist).",
                "status": "PERMISSION_DENIED"
            }})
            .to_string(),
        )
            .into_response()
    });
    let base = serve(app).await;
    let config = format!(
        r#"
[[providers]]
name = "google"
kind = "gemini"
base_url = "{base}"
api_keys = ["AIza-gateway-key-0123456789"]
[[providers.models]]
id = "gemini-up"
"#
    );
    let harness = Harness::start(&config).await;
    let output = harness.ask(Protocol::Gemini, "gemini-up", false).await;
    assert_not_forwarded("gemini passthrough", &output, "gateway-owners-project");
}
