//! Client WebSocket tickets (`POST /v1/ws-ticket`, `?ticket=`) and the
//! `request_id` of the Responses WebSocket's error frames.

mod support;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{
    KEY, LIMITED_KEY, Received, SECOND_KEY, Settings, TestServer, WsClient, http, types, ws_connect,
};
use tokio_tungstenite::tungstenite::Error as WsError;

const DONE: &[&str] = &["response.completed", "error"];

fn user(text: &str) -> Value {
    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
}

fn create(model: &str, text: &str) -> Value {
    json!({"type": "response.create", "model": model, "input": [user(text)]})
}

/// `POST /v1/ws-ticket` with these headers: status, headers, JSON body.
async fn mint(
    server: &TestServer,
    headers: &[(&str, &str)],
) -> (u16, reqwest::header::HeaderMap, Value) {
    let mut request = http().post(server.url("/v1/ws-ticket"));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, headers, body)
}

/// A fresh ticket for `key`.
async fn ticket_for(server: &TestServer, key: &str) -> String {
    let bearer = format!("Bearer {key}");
    let (status, _, body) = mint(server, &[("authorization", bearer.as_str())]).await;
    assert_eq!(status, 201, "{body}");
    body["ticket"].as_str().unwrap().to_string()
}

async fn open(server: &TestServer, path_and_query: &str) -> Result<WsClient, WsError> {
    ws_connect(&server.ws_url(path_and_query), &[]).await
}

fn refused_with(result: Result<WsClient, WsError>, status: u16) {
    match result {
        Err(WsError::Http(response)) => assert_eq!(response.status().as_u16(), status),
        Err(other) => panic!("expected an HTTP {status}, got {other}"),
        Ok(_) => panic!("expected an HTTP {status}, the upgrade succeeded"),
    }
}

#[tokio::test]
async fn a_ticket_opens_one_socket_as_the_key_that_bought_it() {
    let server = TestServer::start().await;
    let bearer = format!("Bearer {KEY}");
    let (status, headers, body) = mint(&server, &[("authorization", bearer.as_str())]).await;
    assert_eq!(status, 201);
    assert_eq!(headers["cache-control"], "no-store");
    assert!(headers.contains_key("x-request-id"));
    assert_eq!(body["expires_in"], 30);
    let ticket = body["ticket"].as_str().unwrap().to_string();
    assert_eq!(ticket.len(), 43, "{ticket}");
    assert_eq!(body.as_object().unwrap().len(), 2, "{body}");
    // Minting is not a request.
    assert!(server.records().is_empty());

    // The other places a key goes work too (and so does a body).
    for (name, value) in [("x-api-key", KEY), ("x-goog-api-key", KEY)] {
        let (status, _, body) = mint(&server, &[(name, value)]).await;
        assert_eq!(status, 201, "{name}: {body}");
    }
    let with_body = http()
        .post(server.url("/v1/ws-ticket"))
        .header("authorization", bearer.as_str())
        .json(&json!({"ignored": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(with_body.status(), 201);

    let mut client = open(&server, &format!("/v1/responses?ticket={ticket}"))
        .await
        .expect("the ticket opens the socket");
    client.send_json(&create("mock-echo", "by ticket")).await;
    let frames = client.read_until(DONE).await;
    assert_eq!(types(&frames).last(), Some(&"response.completed"));
    let records = server.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].client.key_name.as_deref(), Some("tester"));
    let by_key = server
        .gateway
        .authenticate(&switchyard_gateway::PresentedCredentials {
            x_api_key: Some(KEY.to_string()),
            ..Default::default()
        });
    assert_eq!(records[0].client.key_id, by_key.unwrap().key_id);

    // Single use: the same ticket again is a wrong key.
    refused_with(
        open(&server, &format!("/v1/responses?ticket={ticket}")).await,
        401,
    );
    let response = http()
        .get(server.url(&format!("/v1/responses?ticket={ticket}")))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_api_key");
    assert_eq!(body["error"]["message"], "invalid API key");
    // So is one nobody minted.
    refused_with(open(&server, "/v1/responses?ticket=made-up").await, 401);
}

#[tokio::test]
async fn minting_needs_a_valid_key() {
    let server = TestServer::start().await;
    let (status, _, body) = mint(&server, &[]).await;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["code"], "missing_api_key");
    let (status, _, body) = mint(&server, &[("authorization", "Bearer sy-wrong")]).await;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["code"], "invalid_api_key");
    // A ticket does not buy another ticket, nor serve an HTTP route.
    let ticket = ticket_for(&server, KEY).await;
    let response = http()
        .post(server.url(&format!("/v1/ws-ticket?ticket={ticket}")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let response = http()
        .get(server.url(&format!("/v1/models?ticket={ticket}")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    // ... and was not used up by those attempts.
    assert!(
        open(&server, &format!("/v1/responses?ticket={ticket}"))
            .await
            .is_ok()
    );
    // Only POST.
    let response = http()
        .get(server.url("/v1/ws-ticket"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 405);
    assert!(server.records().is_empty());
}

#[tokio::test]
async fn the_keys_allow_list_applies_on_the_socket_and_error_frames_name_their_request() {
    let server = TestServer::start().await;
    // The second key may use the mock models only.
    let ticket = ticket_for(&server, SECOND_KEY).await;
    let mut client = open(&server, &format!("/v1/responses?ticket={ticket}"))
        .await
        .unwrap();
    client.send_json(&create("recorded", "not mine")).await;
    let frame = client.next_json().await;
    assert_eq!(frame["type"], "error", "{frame}");
    assert_eq!(frame["status"], 403);
    assert_eq!(frame["error"]["code"], "model_not_allowed");
    let id = frame["request_id"]
        .as_str()
        .expect("a request id")
        .to_string();
    let record = server.record(&id).await;
    assert_eq!(record.status, 403);
    assert_eq!(record.client.key_name.as_deref(), Some("second"));
    assert!(server.fake.on("/v1/responses").is_empty());

    // The client's own fault: the connection stays usable.
    client.send_json(&create("mock-echo", "mine")).await;
    let frames = client.read_until(DONE).await;
    assert_eq!(types(&frames).last(), Some(&"response.completed"));
}

#[tokio::test]
async fn the_keys_rate_limit_applies_on_the_socket() {
    let server = TestServer::with(Settings {
        limited_rpm: Some(1),
        ..Settings::default()
    })
    .await;
    // Minting costs nothing against the limit.
    let ticket = ticket_for(&server, LIMITED_KEY).await;
    let mut client = open(&server, &format!("/v1/responses?ticket={ticket}"))
        .await
        .unwrap();
    client.send_json(&create("mock-echo", "one")).await;
    let frames = client.read_until(DONE).await;
    assert_eq!(types(&frames).last(), Some(&"response.completed"));

    client.send_json(&create("mock-echo", "two")).await;
    let frame = client.next_json().await;
    assert_eq!(frame["status"], 429, "{frame}");
    assert_eq!(frame["error"]["code"], "rate_limit_exceeded");
    assert!(frame["error"]["headers"]["retry-after"].is_string());
    let id = frame["request_id"].as_str().unwrap().to_string();
    let record = server.record(&id).await;
    assert_eq!(record.status, 429);
    assert_eq!(record.client.key_name.as_deref(), Some("limited"));
    assert!(matches!(
        client.read_close().await,
        Received::Close(Some((1011, _)))
    ));
}

#[tokio::test]
async fn an_error_event_in_a_turns_stream_names_the_turns_request() {
    let server = TestServer::start().await;
    let ticket = ticket_for(&server, KEY).await;
    let mut client = open(&server, &format!("/v1/responses?ticket={ticket}"))
        .await
        .unwrap();
    client.send_json(&create("recorded", "fail midway")).await;
    let frames = client.read_until(&["error"]).await;
    assert_eq!(types(&frames), vec!["response.created", "error"]);
    let error = frames.last().unwrap();
    let id = error["request_id"]
        .as_str()
        .expect("a request id")
        .to_string();
    let record = server.record(&id).await;
    assert_eq!(record.endpoint, "WS /v1/responses");
    assert!(!record.ok);
}

#[tokio::test]
async fn the_realtime_relay_takes_a_ticket_and_never_passes_it_on() {
    let server = TestServer::start().await;
    let ticket = ticket_for(&server, KEY).await;
    let mut client = open(
        &server,
        &format!("/v1/realtime?model=rt&ticket={ticket}&call_id=rtc_9"),
    )
    .await
    .expect("the ticket opens the relay");
    assert!(matches!(client.receive().await, Received::Json(_)));
    let seen = server.fake.on("/v1/realtime");
    assert_eq!(seen[0].query, "model=rt-up&call_id=rtc_9");
    refused_with(
        open(&server, &format!("/v1/realtime?model=rt&ticket={ticket}")).await,
        401,
    );
}

#[tokio::test]
async fn without_required_keys_a_ticket_is_anonymous() {
    let server = TestServer::with(Settings {
        auth_required: false,
        ..Settings::default()
    })
    .await;
    let (status, _, body) = mint(&server, &[]).await;
    assert_eq!(status, 201, "{body}");
    let ticket = body["ticket"].as_str().unwrap().to_string();
    let mut client = open(&server, &format!("/v1/responses?ticket={ticket}"))
        .await
        .unwrap();
    client.send_json(&create("mock-echo", "anyone")).await;
    client.read_until(DONE).await;
    let records = server.records();
    assert_eq!(records[0].client.key_name, None);
    assert_eq!(records[0].client.key_id, None);
}
