//! Review finding GW-12: the `Debug` output of the request types the server
//! hands to the gateway does not print the client's gateway key.
//!
//! "Never log or return API keys" (DESIGN.md, rules for everyone). The
//! gateway asks the server for the client's *complete* header map
//! (`ClientRequest::headers`, `RawRequest::headers`, `WsOpenRequest::headers`;
//! API.md: `request.headers = headers;`), and that map carries the key the
//! client authenticated with — `authorization`, `x-api-key`,
//! `x-goog-api-key` — and, for browser Realtime clients, the
//! `openai-insecure-api-key.<key>` WebSocket subprotocol.
//!
//! `PresentedCredentials` has a hand-written `Debug` that never prints
//! values, as have `Target`, `CredentialView` and `Lease` further down. The
//! three request types derive `Debug`, so the first `tracing::debug!(?request,
//! …)` or `{:?}` in an error path of the server or admin crate writes every
//! client's key into the log (and into the log ring the dashboard shows).

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method};
use std::path::PathBuf;
use switchyard_core::Protocol;
use switchyard_gateway::{
    ClientRequest, Gateway, GatewayOptions, PresentedCredentials, RawRequest, WsOpenRequest,
};
use tokio_util::sync::CancellationToken;

const CLIENT_KEY: &str = "sy-client-key-do-not-log-0123456789";

async fn gateway() -> (Gateway, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("switchyard.toml");
    std::fs::write(
        &path,
        format!(
            "[upstream]\nproxy = \"direct\"\n\n[[auth.keys]]\nkey = \"{CLIENT_KEY}\"\nname = \"tester\"\n\n[[providers]]\nname = \"demo\"\nkind = \"mock\"\n"
        ),
    )
    .unwrap();
    let gateway = Gateway::start(GatewayOptions::new(&path).watch(false))
        .await
        .unwrap();
    (gateway, dir)
}

fn client_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {CLIENT_KEY}")).unwrap(),
    );
    headers.insert("x-api-key", HeaderValue::from_static(CLIENT_KEY));
    headers.insert("x-goog-api-key", HeaderValue::from_static(CLIENT_KEY));
    headers.insert(
        "sec-websocket-protocol",
        HeaderValue::from_str(&format!("realtime, openai-insecure-api-key.{CLIENT_KEY}")).unwrap(),
    );
    headers.insert("user-agent", HeaderValue::from_static("client/1.0"));
    headers
}

#[tokio::test]
async fn request_types_do_not_print_the_clients_key() {
    let (gateway, _dir) = gateway().await;
    let headers = client_headers();
    let identity = gateway
        .authenticate(&PresentedCredentials::from_headers(&headers, None))
        .unwrap();

    let mut generate = ClientRequest::new(
        Protocol::OpenaiChat,
        "POST /v1/chat/completions",
        Bytes::from_static(b"{\"model\":\"mock-echo\",\"messages\":[]}"),
        identity.clone(),
    );
    generate.headers = headers.clone();
    let shown = format!("{generate:?}");
    assert!(
        !shown.contains(CLIENT_KEY),
        "ClientRequest's Debug output prints the client's key: {shown}"
    );

    let raw = RawRequest {
        path: "embeddings".into(),
        method: Method::POST,
        body: Bytes::from_static(b"{\"model\":\"m\",\"input\":\"x\"}"),
        content_type: Some("application/json".into()),
        query: None,
        model: "m".into(),
        headers: headers.clone(),
        identity: identity.clone(),
        client_ip: None,
        endpoint: "POST /v1/embeddings".into(),
        cancel: CancellationToken::new(),
    };
    let shown = format!("{raw:?}");
    assert!(
        !shown.contains(CLIENT_KEY),
        "RawRequest's Debug output prints the client's key: {shown}"
    );

    let ws = WsOpenRequest {
        identity,
        model: "m".into(),
        path_and_query: "realtime?model={model}".into(),
        headers,
        endpoint: "GET /v1/realtime".into(),
        client_ip: None,
        require_kind: None,
    };
    let shown = format!("{ws:?}");
    assert!(
        !shown.contains(CLIENT_KEY),
        "WsOpenRequest's Debug output prints the client's key: {shown}"
    );
    gateway.shutdown().await;
}
