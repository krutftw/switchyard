//! The live-event WebSocket: tickets, frames, topics, lag, shutdown.

mod support;

use futures::StreamExt;
use http::{Method, StatusCode};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use support::{
    App, CLIENT_KEY, Socket, eventually, frame_of, frame_where, next_frame, read, send_json,
    subscribe,
};
use switchyard_core::util::now_unix_ms;
use switchyard_gateway::Gateway;
use switchyard_telemetry::{Event, LogLine};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

fn log_event(message: &str) -> Event {
    Event::Log(Arc::new(LogLine::new(
        now_unix_ms(),
        "info",
        "switchyard::test",
        message,
    )))
}

/// Reads until the server closes the socket and returns the close code.
async fn close_code(socket: &mut Socket) -> Option<u16> {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("the socket closes in time");
        match message {
            Some(Ok(Message::Close(frame))) => return frame.map(|frame| u16::from(frame.code)),
            Some(Ok(_)) => continue,
            Some(Err(_)) | None => return None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tickets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_ticket_opens_exactly_one_socket() {
    let app = App::start().await;

    let (status, body) = app.post("/ws-ticket", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["expires_in"], 30);
    let ticket = body["ticket"].as_str().unwrap().to_string();
    assert_eq!(ticket.len(), 43);
    // A ticket is bought with the secret.
    let (status, _) = read(app.http.post(app.api("/ws-ticket"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let url = app.ws_url(&ticket);
    assert_eq!(app.upgrade_status(&url, &[]).await, 101);
    // Used up.
    assert_eq!(app.upgrade_status(&url, &[]).await, 401);
    // Never issued, or missing.
    assert_eq!(
        app.upgrade_status(&app.ws_url("made-up-ticket"), &[]).await,
        401
    );
    let bare = format!("ws://{}/admin/api/ws", app.addr);
    assert_eq!(app.upgrade_status(&bare, &[]).await, 401);
    // The secret is not a ticket, and is not accepted in its place.
    assert_eq!(
        app.upgrade_status(
            &bare,
            &[("authorization", &format!("Bearer {}", app.secret))]
        )
        .await,
        401
    );

    // A plain GET with a good ticket is not an upgrade.
    let ticket = app.ticket().await;
    let (status, body) = read(app.http.get(app.api(&format!("/ws?ticket={ticket}")))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("WebSocket"),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hello_comes_first_and_stats_follow() {
    let app = App::start().await;
    let (mut socket, hello) = app.live().await;
    assert_eq!(hello["data"]["version"], Gateway::version());
    assert_eq!(
        hello["data"]["topics"],
        json!([
            "hello",
            "request.started",
            "request.finished",
            "log",
            "credential",
            "config.reloaded",
            "stats",
        ])
    );
    assert!(hello["data"]["server_time"].as_i64().unwrap() > 1_600_000_000_000);
    assert_eq!(app.handle.live_connections(), 1);

    // The first tick is immediate.
    let stats = next_frame(&mut socket).await;
    assert_eq!(stats["type"], "stats", "{stats}");
    let data = &stats["data"];
    for field in [
        "at",
        "in_flight",
        "active_streams",
        "ws_connections",
        "rpm",
        "tpm",
        "error_rate_1m",
        "p50_ms",
        "p95_ms",
        "uptime_ms",
    ] {
        assert!(data[field].is_number(), "{field}: {stats}");
    }
    assert_eq!(data["totals"]["requests"], 0);

    // And the next one a second later reflects what happened meanwhile.
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    let stats = frame_where(&mut socket, |frame| {
        frame["type"] == "stats" && frame["data"]["rpm"] == 1
    })
    .await;
    assert_eq!(stats["data"]["totals"]["requests"], 1);
    assert!(stats["data"]["tpm"].as_u64().unwrap() > 0);

    // The JSON ping is answered at once.
    send_json(&mut socket, json!({"type": "ping"})).await;
    let (pong, _) = frame_of(&mut socket, "pong").await;
    assert_eq!(pong, json!({"type": "pong"}));
    // Things the gateway does not understand are ignored, not fatal.
    send_json(&mut socket, json!({"type": "dance"})).await;
    socket.send_text("not json").await;
    send_json(&mut socket, json!({"type": "ping"})).await;
    frame_of(&mut socket, "pong").await;
}

/// `socket.send(Message::text(..))` with the import noise kept out of the
/// tests.
trait SendText {
    async fn send_text(&mut self, text: &str);
}

impl SendText for Socket {
    async fn send_text(&mut self, text: &str) {
        use futures::SinkExt;
        self.send(Message::text(text.to_string()))
            .await
            .expect("the message is sent");
    }
}

#[tokio::test]
async fn every_kind_of_event_is_pushed() {
    let app = App::start().await;
    let (mut socket, _) = app.live().await;
    // Stats would only be noise here.
    subscribe(
        &mut socket,
        &[
            "request.started",
            "request.finished",
            "log",
            "credential",
            "config.reloaded",
        ],
    )
    .await;

    // A request: started, then finished, with the same id.
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    let started = next_frame(&mut socket).await;
    assert_eq!(started["type"], "request.started", "{started}");
    assert_eq!(started["data"]["requested_model"], "mock-echo");
    assert_eq!(started["data"]["client"]["key_name"], "tester");
    assert_eq!(started["data"]["endpoint"], "POST /v1/chat/completions");
    let finished = next_frame(&mut socket).await;
    assert_eq!(finished["type"], "request.finished", "{finished}");
    assert_eq!(finished["data"]["id"], started["data"]["id"]);
    assert_eq!(finished["data"]["status"], 200);
    assert_eq!(finished["data"]["ok"], true);
    assert_eq!(finished["data"]["provider"], "mock");
    assert!(finished["data"]["usage"]["output_tokens"].as_u64().unwrap() > 0);

    // A failed attempt: the credential's new state travels between the two
    // request frames.
    assert!(app.chat(CLIENT_KEY, "mock-error-500").await >= 500);
    let (credential, before) = frame_of(&mut socket, "credential").await;
    assert_eq!(before, ["request.started"]);
    assert_eq!(credential["data"]["provider"], "mock");
    assert_eq!(credential["data"]["credential"]["failures"], 1);
    assert_eq!(
        credential["data"]["credential"]["model_cooldowns"][0]["model"],
        "mock-error-500"
    );
    let (finished, _) = frame_of(&mut socket, "request.finished").await;
    assert_eq!(finished["data"]["ok"], false);
    assert!(
        finished["data"]["error"]["message"].is_string(),
        "{finished}"
    );

    // A log line.
    app.gateway
        .telemetry()
        .publish(log_event("something happened"));
    let log = next_frame(&mut socket).await;
    assert_eq!(log["type"], "log", "{log}");
    assert_eq!(log["data"]["message"], "something happened");
    assert_eq!(log["data"]["level"], "info");
    assert_eq!(log["data"]["target"], "switchyard::test");

    // A configuration change, made through the API.
    let (status, _) = app
        .patch("/settings", json!({"streaming": {"keepalive_secs": 9}}))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (reloaded, _) = frame_of(&mut socket, "config.reloaded").await;
    assert_eq!(reloaded["data"]["ok"], true);
    assert_eq!(reloaded["data"]["message"], "configuration applied");
    assert!(reloaded["data"]["at"].as_i64().unwrap() > 1_600_000_000_000);

    // A configuration that is refused is announced too.
    std::fs::write(&app.config_path, "[server]\nport = 0\n").unwrap();
    let (status, _) = app.post("/reload", json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (rejected, _) = frame_of(&mut socket, "config.reloaded").await;
    assert_eq!(rejected["data"]["ok"], false);
    assert!(
        rejected["data"]["message"]
            .as_str()
            .unwrap()
            .contains("server.port"),
        "{rejected}"
    );
}

#[tokio::test]
async fn subscribe_narrows_what_is_pushed() {
    let app = App::start().await;
    let (mut socket, _) = app.live().await;

    // Only log lines: requests go by unseen.
    subscribe(&mut socket, &["log"]).await;
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    app.gateway.telemetry().publish(log_event("first"));
    send_json(&mut socket, json!({"type": "ping"})).await;
    let (_, before_pong) = frame_of(&mut socket, "pong").await;
    assert_eq!(before_pong, ["log"]);

    // Only stats: nothing else arrives until the next tick.
    subscribe(&mut socket, &["stats"]).await;
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    app.gateway.telemetry().publish(log_event("second"));
    let (stats, before_stats) = frame_of(&mut socket, "stats").await;
    assert_eq!(before_stats, Vec::<String>::new());
    assert_eq!(stats["data"]["totals"]["requests"], 2);

    // Unknown topics are accepted (and never match); non-strings are
    // dropped; an empty list silences everything but direct answers.
    send_json(
        &mut socket,
        json!({"type": "subscribe", "topics": ["log", "from.the.future", 7, "log"]}),
    )
    .await;
    let (ack, _) = frame_of(&mut socket, "subscribed").await;
    assert_eq!(ack["data"]["topics"], json!(["log", "from.the.future"]));
    subscribe(&mut socket, &[]).await;
    app.gateway.telemetry().publish(log_event("third"));
    send_json(&mut socket, json!({"type": "ping"})).await;
    let (_, before_pong) = frame_of(&mut socket, "pong").await;
    assert_eq!(before_pong, Vec::<String>::new());
}

#[tokio::test]
async fn a_subscriber_that_falls_behind_is_told_and_carries_on() {
    // One thread: while this test publishes without yielding, the socket's
    // task cannot run, exactly like a client that is too slow.
    let app = App::start().await;
    let (mut socket, _) = app.live().await;
    subscribe(&mut socket, &["log"]).await;

    let telemetry = app.gateway.telemetry();
    for i in 0..3000 {
        telemetry.publish(log_event(&format!("burst {i}")));
    }
    let (lagged, _) = frame_of(&mut socket, "lagged").await;
    let missed = lagged["data"]["missed"].as_u64().unwrap();
    assert!((1..3000).contains(&missed), "{lagged}");

    // After the notice the stream continues with the newest events, in
    // order, up to the last one.
    let mut last = None;
    let mut received = 0u64;
    while last.as_deref() != Some("burst 2999") {
        let frame = next_frame(&mut socket).await;
        assert_eq!(frame["type"], "log", "{frame}");
        last = frame["data"]["message"].as_str().map(str::to_string);
        received += 1;
    }
    assert_eq!(missed + received, 3000);

    telemetry.publish(log_event("after the burst"));
    let frame = next_frame(&mut socket).await;
    assert_eq!(frame["data"]["message"], "after the burst");
}

// ---------------------------------------------------------------------------
// Ends
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_closes_live_sockets_cleanly() {
    let app = App::start().await;
    let (mut first, _) = app.live().await;
    let (mut second, _) = app.live().await;
    assert_eq!(app.handle.live_connections(), 2);

    app.handle.shutdown();
    for socket in [&mut first, &mut second] {
        assert_eq!(close_code(socket).await, Some(u16::from(CloseCode::Away)));
    }
    let handle = app.handle.clone();
    eventually(|| (handle.live_connections() == 0).then_some(())).await;

    // No new sockets while shutting down; the REST API still answers.
    let ticket = app.ticket().await;
    assert_eq!(app.upgrade_status(&app.ws_url(&ticket), &[]).await, 503);
    assert_eq!(app.get("/status").await.0, StatusCode::OK);
}

#[tokio::test]
async fn a_server_that_stops_takes_its_sockets_with_it() {
    // No handle involved: the server shuts down gracefully, drops the
    // router, and the sockets — which a graceful shutdown does not wait
    // for — are closed properly rather than cut.
    let mut app = App::start().await;
    let (mut socket, _) = app.live().await;
    app.stop
        .take()
        .expect("the server is running")
        .send(())
        .unwrap();
    assert_eq!(
        close_code(&mut socket).await,
        Some(u16::from(CloseCode::Away))
    );
    let handle = app.handle.clone();
    eventually(|| (handle.live_connections() == 0).then_some(())).await;
}

#[tokio::test]
async fn a_client_that_closes_is_forgotten() {
    let app = App::start().await;
    let (mut socket, _) = app.live().await;
    assert_eq!(app.handle.live_connections(), 1);
    socket.close(None).await.unwrap();
    let handle = app.handle.clone();
    eventually(|| (handle.live_connections() == 0).then_some(())).await;
    // The bus no longer carries a subscriber for it.
    let telemetry = app.gateway.telemetry().clone();
    eventually(|| (telemetry.bus().subscriber_count() == 0).then_some(())).await;
}

#[tokio::test]
async fn changing_the_secret_ends_live_sessions() {
    let app = App::start().await;
    let (mut socket, _) = app.live().await;
    let old_ticket = app.ticket().await;

    let (status, body) = app
        .send(
            Method::PATCH,
            "/settings",
            Some(json!({"admin": {"secret": "a-rotated-admin-secret-0000000000"}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Noticed at the next tick, at most a second away.
    assert_eq!(
        close_code(&mut socket).await,
        Some(u16::from(CloseCode::Policy))
    );

    // A ticket bought with the old secret is not to be had any more.
    let (status, _) = app.post("/ws-ticket", json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.upgrade_status(&app.ws_url(&old_ticket), &[]).await, 401);
}

#[tokio::test]
async fn frames_are_json_objects_with_type_and_data() {
    let app = App::start().await;
    let (mut socket, hello) = app.live().await;
    let mut frames: Vec<Value> = vec![hello];
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    app.gateway.telemetry().publish(log_event("shape"));
    while frames.len() < 5 {
        frames.push(next_frame(&mut socket).await);
    }
    for frame in &frames {
        let object = frame.as_object().expect("an object");
        assert!(object["type"].is_string(), "{frame}");
        assert!(object["data"].is_object(), "{frame}");
        assert_eq!(object.len(), 2, "{frame}");
    }
}

#[tokio::test]
async fn the_gateway_pings_every_twenty_seconds() {
    let app = App::start().await;
    let (mut socket, _) = app.live().await;
    subscribe(&mut socket, &[]).await;

    // This is a real TCP integration test. Pausing Tokio time lets its
    // clock run ahead while the kernel is delivering data, so it cannot
    // reliably measure when the client actually receives the ping.
    let before = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Ping(payload))) => {
                    assert!(payload.is_empty());
                    break;
                }
                Some(Ok(Message::Text(_) | Message::Pong(_))) => continue,
                other => panic!("expected a ping, got {other:?}"),
            }
        }
    })
    .await
    .expect("the server sends its first ping within thirty-five seconds");
    let waited = before.elapsed();
    assert!(
        (Duration::from_secs(15)..=Duration::from_secs(30)).contains(&waited),
        "the first ping arrived after {waited:?}"
    );
    send_json(&mut socket, json!({"type": "ping"})).await;
    frame_of(&mut socket, "pong").await;
}
