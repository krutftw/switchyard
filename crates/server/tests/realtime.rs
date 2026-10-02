//! The Realtime relay (`GET /v1/realtime`), against a fake upstream
//! WebSocket that greets and echoes.

mod support;

use futures::SinkExt;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{KEY, Received, TestServer, UPSTREAM_KEY, WsClient, eventually, http, ws_connect};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

async fn connect(server: &TestServer, query: &str) -> WsClient {
    let auth = format!("Bearer {KEY}");
    ws_connect(
        &server.ws_url(&format!("/v1/realtime?{query}")),
        &[("authorization", auth.as_str())],
    )
    .await
    .expect("the upgrade must succeed")
}

fn request_id(client: &WsClient) -> String {
    client.handshake["x-request-id"]
        .to_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn frames_are_relayed_both_ways_untouched() {
    let server = TestServer::start().await;
    let gauges = server.gateway.telemetry().gauges().clone();
    let mut client = connect(&server, "model=rt").await;
    let id = request_id(&client);

    // The upstream speaks first.
    assert_eq!(
        client.receive().await,
        Received::Json(json!({"type": "session.created", "session": {"id": "sess_fake"}}))
    );
    assert_eq!(gauges.ws_connections(), 1);

    client.send_text("hello").await;
    assert_eq!(
        client.receive().await,
        Received::Json(Value::String("echo:hello".into()))
    );
    client
        .socket
        .send(Message::Binary(vec![0u8, 159, 146, 150].into()))
        .await
        .unwrap();
    assert_eq!(
        client.receive().await,
        Received::Binary(vec![0, 159, 146, 150])
    );
    client
        .socket
        .send(Message::Ping(b"p".to_vec().into()))
        .await
        .unwrap();
    assert_eq!(client.receive().await, Received::Pong);

    // The upstream was called with the gateway's credential and the
    // upstream's model id — never with the client's key.
    let seen = server.fake.on("/v1/realtime");
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some(format!("Bearer {UPSTREAM_KEY}").as_str())
    );
    assert_eq!(seen[0].query, "model=rt-up");

    // Usage reported by the upstream is counted for the session.
    client.send_text("respond").await;
    assert!(
        matches!(client.receive().await, Received::Json(done) if done["type"] == "response.done")
    );

    // The client's close reaches the upstream with its code and reason.
    client
        .socket
        .close(Some(CloseFrame {
            code: CloseCode::from(4002),
            reason: "done here".into(),
        }))
        .await
        .unwrap();
    assert!(matches!(client.read_close().await, Received::Close(_)));
    eventually("the upstream to see the close", || {
        server.fake.closes() == vec![(4002, "done here".to_string())]
    })
    .await;
    eventually("the connection to be released", || {
        gauges.ws_connections() == 0
    })
    .await;

    // One request record for the whole session.
    let record = server.record(&id).await;
    assert_eq!(record.endpoint, "GET /v1/realtime");
    assert_eq!(record.status, 101);
    assert_eq!(record.error, None);
    assert_eq!(record.requested_model, "rt");
    assert_eq!(record.upstream_model.as_deref(), Some("rt-up"));
    assert_eq!(record.provider.as_deref(), Some("fake-openai"));
    assert_eq!(record.usage.input_tokens, 15);
    assert_eq!(record.usage.cache_read_tokens, 5);
    assert_eq!(record.usage.output_tokens, 10);
}

#[tokio::test]
async fn browser_clients_authenticate_with_a_subprotocol() {
    let server = TestServer::start().await;
    let offered =
        format!("realtime, openai-insecure-api-key.{KEY}, openai-organization.org_client");
    let mut client = ws_connect(
        &server.ws_url("/v1/realtime?model=rt&call_id=rtc_1"),
        &[("sec-websocket-protocol", offered.as_str())],
    )
    .await
    .expect("the key in the subprotocol must authenticate");
    // A browser aborts unless one of its offers is selected — and the one
    // carrying the key must never be echoed.
    assert_eq!(client.handshake["sec-websocket-protocol"], "realtime");
    assert!(matches!(client.receive().await, Received::Json(_)));

    let seen = server.fake.on("/v1/realtime");
    // The upstream sees neither the client's key nor its account.
    assert_eq!(seen[0].subprotocols.as_deref(), Some("realtime"));
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some(format!("Bearer {UPSTREAM_KEY}").as_str())
    );
    // Other query parameters are passed on.
    assert_eq!(seen[0].query, "model=rt-up&call_id=rtc_1");

    // A wrong key in the subprotocol is still a wrong key.
    let refused = ws_connect(
        &server.ws_url("/v1/realtime?model=rt"),
        &[(
            "sec-websocket-protocol",
            "realtime, openai-insecure-api-key.sy-wrong",
        )],
    )
    .await;
    assert!(matches!(refused, Err(WsError::Http(response)) if response.status() == 401));

    // The key in the query works too, and never reaches the upstream.
    let by_query = ws_connect(
        &server.ws_url(&format!("/v1/realtime?key={KEY}&model=rt")),
        &[],
    )
    .await;
    assert!(by_query.is_ok());
    assert!(
        by_query
            .unwrap()
            .handshake
            .get("sec-websocket-protocol")
            .is_none()
    );
    let seen = server.fake.on("/v1/realtime");
    assert_eq!(seen[1].query, "model=rt-up");
    assert_eq!(seen[1].subprotocols, None);
}

#[tokio::test]
async fn an_upstream_close_reaches_the_client_with_its_code() {
    let server = TestServer::start().await;
    let mut client = connect(&server, "model=rt").await;
    let id = request_id(&client);
    client.receive().await;
    client.send_text("close-me").await;
    assert_eq!(
        client.read_close().await,
        Received::Close(Some((4001, "bye from upstream".to_string())))
    );
    // An application close code is the session ending, not a failure.
    let record = server.record(&id).await;
    assert_eq!(record.status, 101);
    assert_eq!(record.error, None);

    // A server error code is a failure of the upstream.
    let mut client = connect(&server, "model=rt").await;
    let id = request_id(&client);
    client.receive().await;
    client.send_text("fail-me").await;
    assert_eq!(
        client.read_close().await,
        Received::Close(Some((1011, "upstream exploded".to_string())))
    );
    let record = server.record(&id).await;
    let error = record.error.as_ref().expect("the failure is recorded");
    assert_eq!(error.kind, "upstream");
    assert!(error.message.contains("1011"), "{}", error.message);
}

#[tokio::test]
async fn a_client_that_vanishes_ends_the_upstream_session() {
    let server = TestServer::start().await;
    let gauges = server.gateway.telemetry().gauges().clone();
    let mut client = connect(&server, "model=rt").await;
    let id = request_id(&client);
    client.receive().await;
    // No close frame: the connection just goes away.
    drop(client);

    eventually("the upstream session to end", || {
        server.fake.ended_sessions() == 1
    })
    .await;
    eventually("the connection to be released", || {
        gauges.ws_connections() == 0
    })
    .await;
    let record = server.record(&id).await;
    assert_eq!(record.status, 101);
    assert_eq!(
        record.error.as_ref().map(|error| error.kind.as_str()),
        Some("client_disconnect")
    );
}

#[tokio::test]
async fn failures_before_the_upgrade_are_plain_http_errors() {
    let server = TestServer::start().await;
    let auth = format!("Bearer {KEY}");
    let headers = [("authorization", auth.as_str())];

    let status_of = |result: Result<WsClient, WsError>| match result {
        Err(WsError::Http(response)) => {
            let body: Value = response
                .body()
                .as_deref()
                .and_then(|body| serde_json::from_slice(body).ok())
                .unwrap_or(Value::Null);
            (response.status().as_u16(), body)
        }
        Err(other) => panic!("expected an HTTP error, got {other}"),
        Ok(_) => panic!("expected an HTTP error, got a socket"),
    };

    // No key.
    let (status, body) = status_of(ws_connect(&server.ws_url("/v1/realtime?model=rt"), &[]).await);
    assert_eq!(status, 401);
    assert_eq!(body["error"]["type"], "authentication_error");

    // No model.
    let (status, body) = status_of(ws_connect(&server.ws_url("/v1/realtime"), &headers).await);
    assert_eq!(status, 400);
    assert_eq!(body["error"]["param"], "model");

    // A model nobody serves, and one no OpenAI provider serves.
    for model in ["no-such-model", "mock-echo", "embed"] {
        let url = server.ws_url(&format!("/v1/realtime?model={model}"));
        let (status, body) = status_of(ws_connect(&url, &headers).await);
        assert_eq!(status, 404, "{model}");
        assert!(body["error"]["message"].is_string(), "{model}");
    }
    assert!(server.fake.on("/v1/realtime").is_empty());

    // Not an upgrade at all.
    let response = http()
        .get(server.url("/v1/realtime?model=rt"))
        .bearer_auth(KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 426);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "upgrade_required");
}

#[tokio::test]
async fn server_shutdown_closes_both_sides() {
    let mut server = TestServer::start().await;
    let mut client = connect(&server, "model=rt").await;
    client.receive().await;
    server.begin_shutdown();
    assert!(matches!(
        client.read_close().await,
        Received::Close(Some((1001, _)))
    ));
    eventually("the upstream to be told", || {
        server.fake.closes().iter().any(|(code, _)| *code == 1001)
    })
    .await;
    server.stopped().await;
}
