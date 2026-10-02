//! Review: a Responses WebSocket client is dropped as "not answering pings"
//! while it is in the middle of sending a request.
//!
//! `ws/responses.rs` counts a keep-alive ping as unanswered until a whole
//! *message* has been read from the client, and drops the connection at the
//! third tick. A client cannot answer a ping while one of its frames is on
//! the wire: a control frame cannot be placed inside another frame, and a
//! pong the client queues travels behind the bytes it is still sending. So a
//! `response.create` whose upload takes longer than two keep-alive
//! intervals — 30 to 45 s with the default `streaming.keepalive_secs = 15`;
//! a few megabytes of context on a slow uplink — never arrives: the gateway
//! hangs up on a client that is demonstrably alive and sending.
//!
//! (The HTTP path gets this right: `body.rs` gives up on a request body only
//! when *nothing* arrives for a minute.)
//!
//! The failing test uses the smallest keep-alive the configuration allows
//! (1 s), so the upload has to take a little over three seconds. The control
//! test sends the very same bytes at the very same pace with keep-alives
//! off, and is served.

mod support;

use serde_json::json;
use std::time::Duration;
use support::{KEY, Settings, TestServer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// One masked text frame (mask key zero, so the payload goes out as it is).
fn text_frame(payload: &[u8]) -> Vec<u8> {
    assert!((126..65_536).contains(&payload.len()));
    let mut frame = vec![0x81, 0x80 | 126];
    frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame.extend_from_slice(payload);
    frame
}

/// A masked, empty pong.
const PONG: [u8; 6] = [0x8A, 0x80, 0, 0, 0, 0];

/// What came of a `response.create` uploaded over about 3.3 seconds.
struct Upload {
    /// The write that failed, if one did.
    failed: Option<String>,
    /// Everything the server sent afterwards.
    received: String,
    /// Whether a completed turn was recorded.
    recorded: bool,
}

async fn slow_upload(keepalive_secs: u64) -> Upload {
    let server = TestServer::with(Settings {
        keepalive_secs,
        ..Settings::default()
    })
    .await;

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

    let request = json!({
        "type": "response.create",
        "model": "mock-echo",
        "input": [{"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": "slow ".repeat(200)}]}]
    })
    .to_string();
    let frame = text_frame(request.as_bytes());

    // One frame, sent the way a slow uplink delivers it: steadily, over a
    // little more than three seconds. Bytes keep arriving the whole time.
    let pieces = 14;
    let size = frame.len().div_ceil(pieces);
    let mut failed = None;
    for (index, piece) in frame.chunks(size).enumerate() {
        if index > 0 {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if let Err(error) = socket.write_all(piece).await {
            failed = Some(error.to_string());
            break;
        }
    }
    // With the frame out of the way the client answers the pings it got.
    if failed.is_none() {
        let _ = socket.write_all(&PONG).await;
    }

    let mut received = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        let mut buffer = [0u8; 8192];
        loop {
            match socket.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    received.extend_from_slice(&buffer[..read]);
                    if String::from_utf8_lossy(&received).contains("response.completed") {
                        break;
                    }
                }
            }
        }
    })
    .await;
    // The record is published just after the last event; give it a moment
    // when there was a turn at all.
    let served = String::from_utf8_lossy(&received).contains("response.completed");
    let mut recorded = false;
    for _ in 0..if served { 100 } else { 1 } {
        recorded = server
            .records()
            .iter()
            .any(|record| record.endpoint == "WS /v1/responses" && record.status == 200);
        if recorded {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Upload {
        failed,
        received: String::from_utf8_lossy(&received).into_owned(),
        recorded,
    }
}

#[tokio::test]
async fn a_request_that_takes_several_keep_alive_intervals_to_upload_is_still_served() {
    let upload = slow_upload(1).await;
    assert!(
        upload.received.contains("\"response.created\"")
            && upload.received.contains("\"response.completed\""),
        "the gateway hung up on a client that was sending its request the whole time \
         (upload error: {:?}; {} bytes received, no response events)",
        upload.failed,
        upload.received.len()
    );
    assert!(upload.recorded, "no completed turn was recorded");
}

/// The control: the same bytes at the same pace, keep-alives off. Passes
/// today — what kills the upload above is the ping bookkeeping, nothing else.
#[tokio::test]
async fn the_same_upload_is_served_when_keep_alives_are_off() {
    let upload = slow_upload(0).await;
    assert_eq!(upload.failed, None);
    assert!(
        upload.received.contains("\"response.created\"")
            && upload.received.contains("\"response.completed\""),
        "{}",
        upload.received
    );
    assert!(upload.recorded);
}
