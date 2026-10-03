//! Review: with CORS switched off, web pages of other origins can still use
//! the gateway through its WebSockets.
//!
//! `server.cors = false` is how an operator says "no cross-origin use from
//! browsers": no `Access-Control-Allow-Origin`, preflights refused. That
//! protects the HTTP routes only. A WebSocket handshake is not subject to
//! the same-origin policy — the browser sends it to any host and tells the
//! server who is asking in `Origin`; refusing is the server's job. Neither
//! WebSocket route looks at `Origin`.
//!
//! It matters where the network position is the credential:
//! `auth.required = false` ("intended for loopback-only setups"). Any page
//! the operator opens in a browser on that machine can then run
//! `new WebSocket("ws://127.0.0.1:<port>/v1/responses")`, send
//! `response.create`, and read the answers — on the operator's upstream
//! credentials — although CORS is off.
//!
//! Clients that are not browsers send no `Origin` and must keep working, as
//! must a page served from the gateway's own origin.

mod support;

use serde_json::json;
use support::{Settings, TestServer, ws_connect};
use tokio_tungstenite::tungstenite::Error as WsError;

fn closed_to_browsers() -> Settings {
    Settings {
        cors: false,
        auth_required: false,
        ..Settings::default()
    }
}

#[tokio::test]
async fn with_cors_off_a_foreign_origin_cannot_open_the_responses_websocket() {
    let server = TestServer::with(closed_to_browsers()).await;

    // The same page cannot read an HTTP answer …
    let response = support::http()
        .get(server.url("/v1/models"))
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );

    // … and must not get a socket either.
    let foreign = ws_connect(
        &server.ws_url("/v1/responses"),
        &[("origin", "https://evil.example")],
    )
    .await;
    match foreign {
        Err(WsError::Http(response)) => assert_eq!(response.status(), 403),
        Err(other) => panic!("expected an HTTP 403, got {other}"),
        Ok(mut socket) => {
            // Show what the page can do with it.
            socket
                .send_json(&json!({"type": "response.create", "model": "mock-echo",
                    "input": [{"type": "message", "role": "user",
                               "content": [{"type": "input_text", "text": "spend their tokens"}]}]}))
                .await;
            let frames = socket.read_until(&["response.completed", "error"]).await;
            panic!(
                "a page from https://evil.example opened the socket and ran a turn that ended \
                 with `{}`",
                frames.last().unwrap()["type"]
            );
        }
    }
}

#[tokio::test]
async fn with_cors_off_a_foreign_origin_cannot_open_the_realtime_websocket() {
    let server = TestServer::with(closed_to_browsers()).await;
    let foreign = ws_connect(
        &server.ws_url("/v1/realtime?model=rt"),
        &[("origin", "https://evil.example")],
    )
    .await;
    assert!(
        matches!(&foreign, Err(WsError::Http(response)) if response.status() == 403),
        "a page from https://evil.example was relayed to the upstream Realtime API"
    );
    assert!(
        server.fake.on("/v1/realtime").is_empty(),
        "the upstream was dialled for a refused origin"
    );
}

/// What must keep working (passes today; here so that a fix does not
/// overshoot).
#[tokio::test]
async fn clients_without_an_origin_and_same_origin_pages_are_not_affected() {
    let server = TestServer::with(closed_to_browsers()).await;
    assert!(
        ws_connect(&server.ws_url("/v1/responses"), &[])
            .await
            .is_ok(),
        "a client that is not a browser sends no Origin"
    );
    let own = format!("http://{}", server.addr);
    assert!(
        ws_connect(&server.ws_url("/v1/responses"), &[("origin", own.as_str())])
            .await
            .is_ok(),
        "a page of the gateway's own origin"
    );

    // With CORS on, a different origin still needs a client key.
    let open = TestServer::with(Settings {
        cors: true,
        auth_required: false,
        ..Settings::default()
    })
    .await;
    assert!(matches!(
        ws_connect(
            &open.ws_url("/v1/responses"),
            &[("origin", "https://app.example")]
        )
        .await,
        Err(WsError::Http(response)) if response.status() == 403
    ));
    let auth = format!("Bearer {}", support::KEY);
    assert!(
        ws_connect(
            &open.ws_url("/v1/responses"),
            &[("origin", "https://app.example"), ("authorization", &auth)]
        )
        .await
        .is_ok()
    );
}
