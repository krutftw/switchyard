//! Regression test (review finding GW-2): a client's own
//! `OpenAI-Organization` / `OpenAI-Project` header is never sent upstream
//! next to the *gateway's* API key.
//!
//! The official OpenAI SDKs add these headers by themselves when
//! `OPENAI_ORG_ID` / `OPENAI_PROJECT_ID` are set in the environment — which
//! is exactly what a client that used to talk to OpenAI directly and now
//! points its base URL at the gateway still has. The ids name the client's
//! own organisation; the key is the gateway's. OpenAI answers the mismatch
//! with `401 mismatched_organization`, which looks like a rejected
//! credential: forwarded, one request from one client would rest every
//! credential of the provider for `routing.cooldown.auth_secs` and take the
//! provider out of rotation for everybody.
//!
//! (That OpenAI answers this way is simulated by the test's upstream.)

// The shared support module re-exports more than one review file uses.
#[allow(unused_imports)]
mod support;

use axum::Router;
use axum::http::{HeaderMap as AxumHeaders, StatusCode};
use axum::response::{IntoResponse, Response};
use http::HeaderValue;
use serde_json::json;
use std::sync::{Arc, Mutex};
use support::{Harness, Output};
use switchyard_core::Protocol;
use switchyard_scheduler::CredentialStatus;

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// What OpenAI does: a request whose `OpenAI-Organization` (or
/// `OpenAI-Project`) does not belong to the API key is a 401.
fn openai_like(seen: Arc<Mutex<Vec<AxumHeaders>>>) -> Router {
    Router::new().fallback(move |headers: AxumHeaders| {
        let seen = seen.clone();
        async move {
            seen.lock().unwrap().push(headers.clone());
            if headers.contains_key("openai-organization")
                || headers.contains_key("openai-project")
            {
                return (
                    StatusCode::UNAUTHORIZED,
                    [("content-type", "application/json")],
                    json!({"error": {
                        "message": "OpenAI-Organization header should match organization for API key",
                        "type": "invalid_request_error",
                        "param": null,
                        "code": "mismatched_organization"
                    }})
                    .to_string(),
                )
                    .into_response();
            }
            let ok: Response = (
                [("content-type", "application/json")],
                json!({
                    "id": "chatcmpl-1", "object": "chat.completion", "created": 1,
                    "model": "up-chat",
                    "choices": [{"index": 0,
                                 "message": {"role": "assistant", "content": "ok"},
                                 "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })
                .to_string(),
            )
                .into_response();
            ok
        }
    })
}

#[tokio::test]
async fn a_clients_organization_header_cannot_bench_the_gateways_credentials() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = serve(openai_like(seen.clone())).await;
    let harness = Harness::start(&format!(
        r#"
[[providers]]
name = "openai"
kind = "openai"
wire_api = "chat"
base_url = "{base}/v1"
api_keys = ["key-a", "key-b"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#
    ))
    .await;

    // One client still has OPENAI_ORG_ID / OPENAI_PROJECT_ID in its
    // environment; its SDK sends them on every request.
    let mut request = harness.request(Protocol::OpenaiChat, "m", false);
    request.headers.insert(
        "openai-organization",
        HeaderValue::from_static("org-client"),
    );
    request
        .headers
        .insert("openai-project", HeaderValue::from_static("proj_client"));
    let first = Output::read(harness.gateway.generate(request).await).await;

    // Whatever that client is told, the gateway's credentials must still be
    // usable …
    for credential in &harness.gateway.scheduler().snapshot()[0].credentials {
        assert_eq!(
            credential.status,
            CredentialStatus::Ready,
            "a client header rested a credential for {:?}: {:?}",
            credential.cooldown_reason,
            credential.last_error
        );
    }
    // … and every other client must still be served.
    let second = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(
        second.status,
        200,
        "after one request carrying a client's OpenAI-Organization header (answered {}), the model is unavailable: {}",
        first.status,
        String::from_utf8_lossy(&second.body)
    );
}
