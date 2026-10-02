//! Client WebSockets: the upgrade handshake and what the two endpoints
//! ([`responses`], [`realtime`]) share.
//!
//! The handshake is done here rather than with axum's extractor so that the
//! socket is a plain `tokio-tungstenite` stream over the upgraded
//! connection: the same message type as upstream sockets (the Realtime
//! relay forwards frames untouched), and access to the raw connection for
//! the one case the WebSocket layer cannot handle — refusing an oversized
//! message without resetting the connection under the close frame. The
//! connection is also where a client's liveness shows first: bytes arrive on
//! it long before a message is complete ([`Heard`]).

pub(crate) mod realtime;
pub(crate) mod responses;
mod transcript;

use crate::respond;
use axum::body::Body;
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use http::header::{
    CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE,
};
use http::request::Parts;
use http::{HeaderMap, HeaderValue, StatusCode};
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use switchyard_core::{ApiError, Protocol};
use switchyard_gateway::Gateway;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

/// A client's WebSocket.
pub(crate) type ClientSocket = WebSocketStream<Heard<TokioIo<Upgraded>>>;

/// A connection that counts what arrives on it.
///
/// A WebSocket only reports complete messages, and a client can answer a
/// ping only between its own frames. A client that is slowly uploading a
/// large request is therefore silent as far as messages go, for as long as
/// the upload takes — while its bytes keep arriving here. Counting them is
/// how a session tells such a client from one that is gone.
pub(crate) struct Heard<S> {
    inner: S,
    bytes: u64,
}

impl<S> Heard<S> {
    fn new(inner: S) -> Self {
        Heard { inner, bytes: 0 }
    }

    /// How many bytes the peer has sent so far (wrapping). Only ever
    /// compared with an earlier reading: any change means the peer is
    /// alive.
    pub(crate) fn bytes_heard(&self) -> u64 {
        self.bytes
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Heard<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let outcome = Pin::new(&mut self.inner).poll_read(cx, buf);
        let read = buf.filled().len().saturating_sub(before);
        self.bytes = self.bytes.wrapping_add(read as u64);
        outcome
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Heard<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Close code: the server is going away.
pub(crate) const GOING_AWAY: u16 = 1001;
/// Close code: a message was larger than this server accepts.
pub(crate) const TOO_BIG: u16 = 1009;
/// Close code: the server hit a condition that keeps it from continuing.
pub(crate) const INTERNAL_ERROR: u16 = 1011;

/// How long a closing socket is given to answer the close frame.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// How long the rest of a refused, oversized message is read away at most.
const DRAIN_LIMIT: Duration = Duration::from_secs(10);

/// How long such a client may send nothing before it is taken to be done.
const DRAIN_QUIET: Duration = Duration::from_millis(500);

/// The longest close reason a frame can carry, in bytes.
const MAX_REASON_BYTES: usize = 123;

/// Whether a comma-separated header contains `token` (case-insensitively).
fn has_token(headers: &HeaderMap, name: http::HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|item| item.trim().eq_ignore_ascii_case(token))
}

/// A validated upgrade request, ready to be accepted.
pub(crate) struct Handshake {
    accept: HeaderValue,
    on_upgrade: OnUpgrade,
}

/// Checks that a request is a WebSocket upgrade this server can perform.
/// `Err` is the HTTP response to send instead: `426 Upgrade Required` for a
/// plain request, `400` for a broken handshake.
pub(crate) fn handshake(gateway: &Gateway, parts: &mut Parts) -> Result<Handshake, Box<Response>> {
    let refuse = |status: u16, message: &str, code: &str| {
        let error = ApiError::invalid_request(message)
            .with_status(status)
            .with_code(code);
        let mut response = respond::error(gateway, Protocol::OpenaiResponses, &error);
        if status == StatusCode::UPGRADE_REQUIRED.as_u16() {
            let headers = response.headers_mut();
            headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
            headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
            headers.insert(SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
        }
        Box::new(response)
    };

    let upgrading = has_token(&parts.headers, CONNECTION, "upgrade")
        && has_token(&parts.headers, UPGRADE, "websocket");
    if !upgrading {
        return Err(refuse(
            426,
            "this endpoint is a WebSocket: connect with `Connection: Upgrade` and `Upgrade: websocket`",
            "upgrade_required",
        ));
    }
    if !has_token(&parts.headers, SEC_WEBSOCKET_VERSION, "13") {
        return Err(refuse(
            426,
            "unsupported WebSocket version; this server speaks version 13",
            "upgrade_required",
        ));
    }
    let key = parts
        .headers
        .get(SEC_WEBSOCKET_KEY)
        .map(|key| key.as_bytes().to_vec())
        .filter(|key| !key.is_empty());
    let Some(key) = key else {
        return Err(refuse(
            400,
            "the WebSocket handshake has no Sec-WebSocket-Key",
            "invalid_handshake",
        ));
    };
    let Ok(accept) = HeaderValue::from_str(&derive_accept_key(&key)) else {
        return Err(refuse(
            400,
            "the WebSocket handshake has no usable Sec-WebSocket-Key",
            "invalid_handshake",
        ));
    };
    // Absent on connections that cannot be upgraded (HTTP/2).
    let Some(on_upgrade) = parts.extensions.remove::<OnUpgrade>() else {
        return Err(refuse(
            426,
            "WebSockets are served over HTTP/1.1 only",
            "upgrade_required",
        ));
    };
    Ok(Handshake { accept, on_upgrade })
}

impl Handshake {
    /// Accepts the upgrade: returns the `101` response and, once hyper has
    /// written it, runs `session` on the socket in a task of its own.
    ///
    /// `subprotocol` is echoed as `Sec-WebSocket-Protocol`; `max_message`
    /// bounds a single message (and frame) from the client.
    pub(crate) fn accept<F, Fut>(
        self,
        subprotocol: Option<HeaderValue>,
        max_message: usize,
        session: F,
    ) -> Response
    where
        F: FnOnce(ClientSocket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let config = WebSocketConfig::default()
            .max_message_size(Some(max_message))
            .max_frame_size(Some(max_message));
        let on_upgrade = self.on_upgrade;
        tokio::spawn(async move {
            match on_upgrade.await {
                Ok(upgraded) => {
                    let socket = WebSocketStream::from_raw_socket(
                        Heard::new(TokioIo::new(upgraded)),
                        Role::Server,
                        Some(config),
                    )
                    .await;
                    session(socket).await;
                }
                Err(error) => {
                    tracing::debug!(%error, "a WebSocket upgrade did not complete");
                }
            }
        });

        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        let headers = response.headers_mut();
        headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(SEC_WEBSOCKET_ACCEPT, self.accept);
        if let Some(subprotocol) = subprotocol {
            headers.insert(http::header::SEC_WEBSOCKET_PROTOCOL, subprotocol);
        }
        response
    }
}

/// `reason` cut to what fits into a close frame, on a character boundary.
fn fit_reason(reason: &str) -> &str {
    if reason.len() <= MAX_REASON_BYTES {
        return reason;
    }
    let mut end = MAX_REASON_BYTES;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    &reason[..end]
}

/// A close frame.
pub(crate) fn close_frame(code: u16, reason: &str) -> CloseFrame {
    CloseFrame {
        code: CloseCode::from(code),
        reason: fit_reason(reason).to_string().into(),
    }
}

/// Close codes that only exist as local signals and must not be written
/// into a frame (no status received, abnormal closure, TLS failure), plus
/// anything outside the ranges an endpoint may send: replaced by a normal
/// closure.
pub(crate) fn sendable_code(code: u16) -> u16 {
    match code {
        1005 | 1006 | 1015 => 1000,
        1000..=1014 | 3000..=4999 => code,
        _ => 1000,
    }
}

/// Reads until the peer has closed, for at most [`CLOSE_WAIT`]. Reading is
/// what lets the WebSocket layer write a pending close reply and see the
/// peer's.
pub(crate) async fn await_close<S>(socket: &mut S)
where
    S: futures::Stream<Item = Result<Message, WsError>> + Unpin,
{
    let _ = tokio::time::timeout(CLOSE_WAIT, async {
        while let Some(Ok(_)) = socket.next().await {}
    })
    .await;
}

/// Sends a close frame, giving up after [`CLOSE_WAIT`] on a peer that does
/// not take it. False when the frame could not be sent.
pub(crate) async fn send_close<S>(socket: &mut S, frame: Option<CloseFrame>) -> bool
where
    S: futures::Sink<Message, Error = WsError> + Unpin,
{
    matches!(
        tokio::time::timeout(CLOSE_WAIT, socket.send(Message::Close(frame))).await,
        Ok(Ok(()))
    )
}

/// Closes a healthy socket with `code` and waits briefly for the client to
/// acknowledge, so the close frame is not lost to a reset.
pub(crate) async fn close(socket: &mut ClientSocket, code: u16, reason: &str) {
    if send_close(socket, Some(close_frame(code, reason))).await {
        await_close(socket).await;
    }
}

/// Refuses a client whose message exceeded the size limit: close code 1009.
///
/// The WebSocket layer gave up in the middle of the offending message, so
/// its read side is unusable and the rest of the message may still be on
/// its way. Closing the connection with that data unread would make the
/// operating system reset it — and take the close frame with it — so the
/// raw connection is read away first: until the client hangs up, falls
/// silent (it has sent what it had, its own close frame included), or has
/// been at it for [`DRAIN_LIMIT`].
pub(crate) async fn refuse_oversized(socket: &mut ClientSocket) {
    let frame = close_frame(TOO_BIG, "message too big");
    if !send_close(socket, Some(frame)).await {
        return;
    }
    let io = socket.get_mut();
    let mut scratch = vec![0u8; 16 * 1024];
    let _ = tokio::time::timeout(DRAIN_LIMIT, async {
        while let Ok(Ok(read)) = tokio::time::timeout(DRAIN_QUIET, io.read(&mut scratch)).await {
            if read == 0 {
                break;
            }
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive, Upgrade"));
        headers.insert(UPGRADE, HeaderValue::from_static("WebSocket"));
        assert!(has_token(&headers, CONNECTION, "upgrade"));
        assert!(has_token(&headers, UPGRADE, "websocket"));
        assert!(!has_token(&headers, CONNECTION, "close"));
        assert!(!has_token(&headers, SEC_WEBSOCKET_VERSION, "13"));
    }

    #[test]
    fn close_reasons_fit_a_frame() {
        assert_eq!(fit_reason("bye"), "bye");
        let long = "é".repeat(100);
        let cut = fit_reason(&long);
        assert!(cut.len() <= MAX_REASON_BYTES);
        assert_eq!(cut.len(), 122, "cut on a character boundary");
        let frame = close_frame(1011, &long);
        assert_eq!(u16::from(frame.code), 1011);
        assert_eq!(frame.reason.as_str(), cut);
    }

    #[test]
    fn only_real_close_codes_are_sent() {
        assert_eq!(sendable_code(1000), 1000);
        assert_eq!(sendable_code(1001), 1001);
        assert_eq!(sendable_code(1005), 1000);
        assert_eq!(sendable_code(1006), 1000);
        assert_eq!(sendable_code(1015), 1000);
        assert_eq!(sendable_code(1011), 1011);
        assert_eq!(sendable_code(4000), 4000);
        assert_eq!(sendable_code(999), 1000);
        assert_eq!(sendable_code(2000), 1000);
        assert_eq!(sendable_code(5000), 1000);
    }
}
