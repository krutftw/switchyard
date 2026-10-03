//! Current client access policy applies to HTTP and already-open sockets.
mod support;

use std::time::Duration;
use support::{KEY, Received, Settings, TestServer, http, ws_connect};
use tokio_tungstenite::tungstenite::Error as WsError;

#[tokio::test]
async fn anonymous_rebinding_names_are_rejected_and_keyed_proxies_work() {
    let server = TestServer::with(Settings {
        auth_required: false,
        ..Settings::default()
    })
    .await;
    for key in [false, true] {
        let mut request = http().post(server.url("/v1/chat/completions"))
            .header("host", "unconfigured.example:8317")
            .header("origin", "http://unconfigured.example:8317")
            .header("sec-fetch-site", "same-origin")
            .json(&serde_json::json!({"model":"mock-echo","messages":[{"role":"user","content":"hello"}]}));
        if key {
            request = request.bearer_auth(KEY);
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            if key { 200 } else { 403 }
        );
    }
    let refused = ws_connect(
        &server.ws_url("/v1/realtime?model=rt"),
        &[("origin", "https://unconfigured.example")],
    )
    .await;
    assert!(matches!(refused, Err(WsError::Http(response)) if response.status() == 403));
    assert!(server.fake.on("/v1/realtime").is_empty());
}

#[tokio::test]
async fn both_open_socket_types_close_when_their_key_is_disabled() {
    let server = TestServer::start().await;
    let auth = format!("Bearer {KEY}");
    let mut responses = ws_connect(&server.ws_url("/v1/responses"), &[("authorization", &auth)])
        .await
        .unwrap();
    let mut realtime = ws_connect(
        &server.ws_url("/v1/realtime?model=rt"),
        &[("authorization", &auth)],
    )
    .await
    .unwrap();
    assert!(matches!(realtime.receive().await, Received::Json(_)));
    let mut applied = server.gateway.telemetry().subscribe();
    server
        .gateway
        .config_store()
        .update(|config| {
            config
                .auth
                .keys
                .iter_mut()
                .find(|key| key.key == KEY)
                .unwrap()
                .enabled = false;
            Ok(())
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if applied.recv().await.unwrap().topic() == "config.reloaded" {
                break;
            }
        }
    })
    .await
    .unwrap();
    for socket in [&mut responses, &mut realtime] {
        assert!(matches!(
            socket.receive().await,
            Received::Close(Some((1008, _)))
        ));
    }
}
