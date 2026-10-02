//! When the Responses WebSocket gives up on a client that does not answer
//! its keep-alive pings — and when it must not.
//!
//! The client here is a bare TCP connection that speaks just enough of the
//! protocol to send frames at its own pace and that never answers a ping by
//! itself, which no WebSocket library lets a test do.
//!
//! The smallest keep-alive interval the configuration allows is one second
//! and a client is given up on at the third unanswered ping, so each of
//! these tests takes a little over three seconds; they run side by side.

mod support;

use serde_json::json;
use std::time::{Duration, Instant};
use support::{KEY, Settings, TestServer, eventually};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A masked, empty pong (mask key zero).
const PONG: [u8; 6] = [0x8A, 0x80, 0, 0, 0, 0];

async fn server() -> TestServer {
    TestServer::with(Settings {
        keepalive_secs: 1,
        ..Settings::default()
    })
    .await
}

/// Upgrades a fresh connection to a Responses WebSocket.
async fn connect(server: &TestServer) -> TcpStream {
    let mut socket = TcpStream::connect(server.addr).await.unwrap();
    let upgrade = format!(
        "GET /v1/responses HTTP/1.1\r\nhost: gateway\r\nauthorization: Bearer {KEY}\r\n\
         connection: Upgrade\r\nupgrade: websocket\r\nsec-websocket-version: 13\r\n\
         sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    socket.write_all(upgrade.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    assert!(
        head.starts_with(b"HTTP/1.1 101"),
        "{}",
        String::from_utf8_lossy(&head)
    );
    socket
}

/// One masked text frame (mask key zero, so the payload goes out as it is).
fn text_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x81];
    match payload.len() {
        length @ 0..126 => frame.push(0x80 | length as u8),
        length @ 126..65_536 => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(length as u16).to_be_bytes());
        }
        length => panic!("{length} bytes is more than these tests send"),
    }
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame.extend_from_slice(payload);
    frame
}

fn request(model: &str, text: &str) -> Vec<u8> {
    text_frame(
        json!({"type": "response.create", "model": model,
               "input": [{"type": "message", "role": "user",
                          "content": [{"type": "input_text", "text": text}]}]})
        .to_string()
        .as_bytes(),
    )
}

/// Reads until the server has sent `count` `response.completed` events, the
/// connection ends, or `within` is over. Returns everything received (the
/// server's frames are unmasked, so events are readable as they are) and
/// whether the connection ended.
async fn read_completed(socket: &mut TcpStream, count: usize, within: Duration) -> (String, bool) {
    let mut received = Vec::new();
    let mut ended = false;
    let _ = tokio::time::timeout(within, async {
        let mut buffer = [0u8; 8192];
        loop {
            match socket.read(&mut buffer).await {
                Ok(0) | Err(_) => {
                    ended = true;
                    break;
                }
                Ok(read) => {
                    received.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&received);
                    if text.matches("\"response.completed\"").count() >= count {
                        break;
                    }
                }
            }
        }
    })
    .await;
    (String::from_utf8_lossy(&received).into_owned(), ended)
}

/// The turns recorded with `status`.
fn turns(server: &TestServer, status: u16) -> usize {
    server
        .records()
        .iter()
        .filter(|record| record.endpoint == "WS /v1/responses" && record.status == status)
        .count()
}

/// Between turns a client need not be reading its socket at all — many only
/// come back to it when they have something to send — so pings it leaves
/// unanswered while idle do not cost it the connection.
#[tokio::test]
async fn an_idle_client_that_answers_no_pings_keeps_its_connection() {
    let server = server().await;
    let mut socket = connect(&server).await;

    // Three keep-alive intervals of silence and then some.
    tokio::time::sleep(Duration::from_millis(3_400)).await;
    socket
        .write_all(&request("mock-echo", "back after a pause"))
        .await
        .expect("the connection must still be there");
    let (received, _) = read_completed(&mut socket, 1, Duration::from_secs(3)).await;
    assert!(
        received.contains("back after a pause") && received.contains("\"response.completed\""),
        "the request after the pause was not served: {received}"
    );
    eventually("the turn to be recorded", || turns(&server, 200) == 1).await;
}

/// During a turn the client is being sent events; one that reads them
/// answers pings, and one that does not is gone. Its turn is cancelled
/// instead of running on for nobody.
#[tokio::test]
async fn a_client_that_falls_silent_during_a_turn_is_dropped() {
    let server = server().await;
    let gauges = server.gateway.telemetry().gauges().clone();
    let mut socket = connect(&server).await;
    // 600 words at three words every 300 ms: a minute, left alone.
    socket
        .write_all(&request("mock-slow", &"word ".repeat(600)))
        .await
        .unwrap();

    let started = Instant::now();
    let (received, ended) = read_completed(&mut socket, 1, Duration::from_secs(8)).await;
    let waited = started.elapsed();
    assert!(ended, "the connection was kept: {received}");
    assert!(received.contains("\"response.created\""));
    assert!(!received.contains("\"response.completed\""));
    // Pings at 1 s and 2 s went unanswered; the third tick ends it.
    assert!(
        waited >= Duration::from_millis(2_500) && waited < Duration::from_secs(6),
        "{waited:?}"
    );
    eventually("the turn and the connection to be released", || {
        gauges.ws_connections() == 0 && gauges.active_streams() == 0 && gauges.in_flight() == 0
    })
    .await;
    assert_eq!(turns(&server, 499), 1, "the turn is recorded as abandoned");
}

/// A client cannot answer a ping while one of its own frames is on the
/// wire. Bytes that keep arriving are as good an answer: a request that
/// takes several keep-alive intervals to upload — here during a turn, when
/// patience is shortest — is served like any other.
#[tokio::test]
async fn a_request_uploaded_slowly_during_a_turn_is_served() {
    let server = server().await;
    let mut socket = connect(&server).await;
    // 39 words: thirteen chunks, close to four seconds.
    socket
        .write_all(&request("mock-slow", &"word ".repeat(39)))
        .await
        .unwrap();

    // The next request, sent the way a slow uplink delivers it: steadily,
    // over a little more than three seconds, with no pong in between.
    // (A short answer: nobody answers pings while it is streamed, either.)
    let frame = request("mock-echo", "second request");
    let pieces = 14;
    let size = frame.len().div_ceil(pieces);
    for (index, piece) in frame.chunks(size).enumerate() {
        if index > 0 {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        socket
            .write_all(piece)
            .await
            .expect("the gateway hung up on a client that was sending the whole time");
    }
    // With the frame out of the way the client answers the pings it got.
    socket.write_all(&PONG).await.unwrap();

    let (received, ended) = read_completed(&mut socket, 2, Duration::from_secs(5)).await;
    assert!(!ended, "the connection was dropped: {received}");
    assert_eq!(
        received.matches("\"response.completed\"").count(),
        2,
        "{received}"
    );
    assert!(received.contains("second request"), "{received}");
    eventually("both turns to be recorded", || turns(&server, 200) == 2).await;
}
