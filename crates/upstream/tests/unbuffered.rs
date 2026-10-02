//! `UpstreamClient::send_unbuffered`: the same call as `send`, with the body
//! of a successful answer handed over as a stream so the caller can bound
//! what it keeps in memory.

mod common;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use common::{KEY, client, openai, serve, timeouts};
use futures::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;
use switchyard_core::FailureClass;
use switchyard_upstream::{Operation, Timeouts, UpstreamBody};

const GENERATE: Operation = Operation::Generate { stream: false };
const STREAM: Operation = Operation::Generate { stream: true };

fn body() -> Bytes {
    Bytes::from(json!({"model": "gpt-test", "messages": []}).to_string())
}

#[tokio::test]
async fn a_non_streaming_answer_arrives_as_a_stream_with_the_same_bytes() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|headers: HeaderMap| async move {
            let auth = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            (
                [
                    (header::CONTENT_TYPE, "application/json"),
                    (header::SET_COOKIE, "session=abc"),
                ],
                json!({"auth": auth, "padding": "x".repeat(200_000)}).to_string(),
            )
        }),
    );
    let addr = serve(app).await;
    let response = client()
        .send_unbuffered(
            &openai(addr),
            &GENERATE,
            body(),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.media_type().as_deref(), Some("application/json"));
    // Headers are filtered exactly as `send` filters them.
    assert!(response.headers.get("set-cookie").is_none());
    assert!(response.body.is_stream(), "the body must not be buffered");

    let bytes = response.body.collect().await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["auth"], format!("Bearer {KEY}"));
    assert_eq!(json["padding"].as_str().unwrap().len(), 200_000);
}

#[tokio::test]
async fn the_caller_can_stop_reading_a_body_that_is_too_large() {
    // An endless body: `send` would never return; `send_unbuffered` returns
    // as soon as the headers arrive and the caller gives up when it has
    // seen enough.
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let chunks = futures::stream::repeat_with(|| {
                Ok::<_, std::convert::Infallible>(Bytes::from(vec![b'a'; 16 * 1024]))
            });
            Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(chunks))
                .unwrap()
        }),
    );
    let addr = serve(app).await;
    let response = client()
        .send_unbuffered(
            &openai(addr),
            &GENERATE,
            body(),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap();
    let UpstreamBody::Stream(mut stream) = response.body else {
        panic!("expected a stream");
    };
    let limit = 256 * 1024;
    let mut seen = 0usize;
    while let Some(chunk) = stream.next().await {
        seen += chunk.unwrap().len();
        if seen > limit {
            break;
        }
    }
    assert!(seen > limit);
    // Dropping the stream closes the connection; nothing else to assert
    // beyond the test finishing.
}

#[tokio::test]
async fn failures_are_classified_like_send() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                StatusCode::TOO_MANY_REQUESTS,
                [
                    (header::CONTENT_TYPE, "application/json"),
                    (header::RETRY_AFTER, "7"),
                ],
                json!({"error": {"message": "slow down", "type": "rate_limit_error"}}).to_string(),
            )
                .into_response()
        }),
    );
    let addr = serve(app).await;
    let error = client()
        .send_unbuffered(
            &openai(addr),
            &GENERATE,
            body(),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 429);
    assert_eq!(error.class, FailureClass::RateLimit);
    assert_eq!(error.retry_after_ms, Some(7_000));
    assert_eq!(error.info.message, "slow down");
    assert!(error.body.unwrap().contains("slow down"));
}

#[tokio::test]
async fn the_request_deadline_still_covers_the_body() {
    // Headers at once, then a body that stalls for longer than the deadline.
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let chunks = futures::stream::unfold(0u8, |state| async move {
                match state {
                    0 => Some((
                        Ok::<_, std::convert::Infallible>(Bytes::from_static(b"{\"partial\":")),
                        1,
                    )),
                    _ => {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        None
                    }
                }
            });
            Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(chunks))
                .unwrap()
        }),
    );
    let addr = serve(app).await;
    let limits = Timeouts {
        connect: Duration::from_secs(5),
        request: Duration::from_millis(400),
    };
    let response = client()
        .send_unbuffered(&openai(addr), &GENERATE, body(), &HeaderMap::new(), limits)
        .await
        .unwrap();
    let error = response.body.collect().await.unwrap_err();
    assert_eq!(error.class, FailureClass::Transport);
    assert_eq!(error.status, 408, "{}", error.info.message);
    assert!(
        error.info.message.starts_with("timeout:"),
        "{}",
        error.info.message
    );
}

#[tokio::test]
async fn a_streaming_call_behaves_exactly_like_send() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|headers: HeaderMap| async move {
            let accept = headers
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                format!("data: {accept}\n\ndata: [DONE]\n\n"),
            )
        }),
    );
    let addr = serve(app).await;
    let response = client()
        .send_unbuffered(
            &openai(addr),
            &STREAM,
            body(),
            &HeaderMap::new(),
            // A request deadline that a stream must not be bound by.
            Timeouts {
                connect: Duration::from_secs(5),
                request: Duration::from_millis(1),
            },
        )
        .await
        .unwrap();
    assert!(response.body.is_stream());
    let text = String::from_utf8(response.body.collect().await.unwrap().to_vec()).unwrap();
    assert_eq!(text, "data: text/event-stream\n\ndata: [DONE]\n\n");
}
