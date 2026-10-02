//! Regression tests (review finding GW-1): a 404 / 405 from an *optional*
//! endpoint (a raw side endpoint, an upstream WebSocket handshake) says
//! "this upstream has no such endpoint", not "this credential cannot serve
//! this model".
//!
//! `switchyard_upstream::classify` documents the hand-off: "Whether a 404 or
//! 405 from an optional endpoint (token counting, a raw side endpoint)
//! should count against the credential at all is for the caller to decide:
//! it knows which operation it sent." Reported as it is, such an answer
//! would rest the *model* on that credential for
//! `routing.cooldown.model_not_found_secs` (12 hours by default) — for
//! ordinary generation requests too. `raw` and `open_upstream_ws` therefore
//! pass the answer on without telling the scheduler. (`tests/hardening.rs`
//! covers the other refusals of optional endpoints.)

// The shared support module re-exports more than one review file uses.
#[allow(unused_imports)]
mod support;

use axum::Router;
use axum::http::{Method as AxumMethod, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method};
use serde_json::json;
use support::{CLIENT_KEY, Harness, Output};
use switchyard_core::Protocol;
use switchyard_core::config::ProviderKind;
use switchyard_gateway::{RawRequest, WsOpenRequest};
use switchyard_scheduler::CredentialStatus;
use tokio_util::sync::CancellationToken;

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// An OpenAI-compatible server that implements chat completions and nothing
/// else, the way many local servers and thin proxies do: every other path
/// is the web framework's plain 404.
async fn chat_only(method: AxumMethod, uri: Uri) -> Response {
    if method == AxumMethod::POST && uri.path() == "/v1/chat/completions" {
        return (
            [("content-type", "application/json")],
            json!({
                "id": "chatcmpl-1", "object": "chat.completion", "created": 1,
                "model": "up-chat",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                             "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })
            .to_string(),
        )
            .into_response();
    }
    (
        StatusCode::NOT_FOUND,
        [("content-type", "application/json")],
        json!({"detail": "Not Found"}).to_string(),
    )
        .into_response()
}

fn config(kind: &str, base: &str) -> String {
    format!(
        r#"
[[providers]]
name = "local"
kind = "{kind}"
wire_api = "chat"
base_url = "{base}/v1"
api_keys = ["key-local-1"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#
    )
}

fn raw_request(harness: &Harness, path: &str, model: &str) -> RawRequest {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {CLIENT_KEY}")).unwrap(),
    );
    RawRequest {
        path: path.to_string(),
        method: Method::POST,
        body: Bytes::from(json!({"input": "hello", "model": model}).to_string()),
        content_type: Some("application/json".to_string()),
        query: None,
        model: model.to_string(),
        headers,
        identity: harness.identity(),
        client_ip: None,
        endpoint: format!("POST /v1/{path}"),
        cancel: CancellationToken::new(),
    }
}

fn assert_model_still_served(harness: &Harness, what: &str) {
    let credential = &harness.gateway.scheduler().snapshot()[0].credentials[0];
    assert_eq!(
        credential.status,
        CredentialStatus::Ready,
        "{what} rested the credential: {credential:?}"
    );
    assert!(
        credential.model_cooldowns.is_empty(),
        "{what} rested the model: {:?}",
        credential.model_cooldowns
    );
}

/// One client asks for `/v1/moderations` on an upstream that has no such
/// endpoint. The client gets the upstream's 404, and chat requests for the
/// model keep being served (rather than refused with "all credentials are
/// cooling down; retry in 43200s").
#[tokio::test]
async fn a_missing_raw_endpoint_does_not_take_the_model_out_of_service() {
    let base = serve(Router::new().fallback(chat_only)).await;
    let harness = Harness::start(&config("openai-compat", &base)).await;
    assert_eq!(
        harness.ask(Protocol::OpenaiChat, "m", false).await.status,
        200
    );

    let raw = Output::read(
        harness
            .gateway
            .raw(raw_request(&harness, "moderations", "m"))
            .await,
    )
    .await;
    assert_eq!(raw.status, 404, "the upstream's answer is passed on");

    assert_model_still_served(&harness, "a 404 from a raw side endpoint");
    let chat = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(
        chat.status,
        200,
        "chat must still work after a raw 404: {}",
        String::from_utf8_lossy(&chat.body)
    );
}

/// The same through `open_upstream_ws`: an upstream (or the reverse proxy
/// in front of it) that does not serve the WebSocket endpoint answers the
/// handshake with 404. The model stays available for HTTP requests —
/// including the HTTP fallback the server uses for that client.
#[tokio::test]
async fn a_refused_websocket_handshake_does_not_take_the_model_out_of_service() {
    let base = serve(Router::new().fallback(chat_only)).await;
    let harness = Harness::start(&config("openai", &base)).await;

    let error = harness
        .gateway
        .open_upstream_ws(WsOpenRequest {
            identity: harness.identity(),
            model: "m".to_string(),
            path_and_query: "responses".to_string(),
            headers: HeaderMap::new(),
            endpoint: "GET /v1/responses".to_string(),
            client_ip: None,
            require_kind: Some(ProviderKind::Openai),
        })
        .await
        .expect_err("the upstream has no WebSocket endpoint");
    assert!(error.status >= 400, "{error:?}");

    assert_model_still_served(&harness, "a 404 on the WebSocket handshake");
    let chat = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(
        chat.status,
        200,
        "chat must still work after a refused handshake: {}",
        String::from_utf8_lossy(&chat.body)
    );
}
