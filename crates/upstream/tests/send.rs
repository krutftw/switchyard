//! End-to-end tests of `UpstreamClient::send` against a local HTTP server.

mod common;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use common::{KEY, client, dead_addr, openai, serve, target, timeouts};
use futures::StreamExt;
use serde_json::{Value, json};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use switchyard_core::config::{ProviderKind, ProxySetting};
use switchyard_core::{FailureClass, Protocol};
use switchyard_upstream::{Operation, Timeouts, UpstreamBody, resolve_proxy};
use tokio::sync::Notify;

const GENERATE: Operation = Operation::Generate { stream: false };
const STREAM: Operation = Operation::Generate { stream: true };

fn body() -> Bytes {
    Bytes::from(
        json!({"model": "gpt-test", "messages": [{"role": "user", "content": "hi"}]}).to_string(),
    )
}

/// Answers with what it received, so tests can assert on the wire request.
async fn echo(headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    let seen: serde_json::Map<String, Value> = headers
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                Value::String(v.to_str().unwrap_or("").to_string()),
            )
        })
        .collect();
    let payload = json!({
        "path": uri.path(),
        "query": uri.query(),
        "headers": seen,
        "body": serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
    });
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::SET_COOKIE, "session=abc"),
            (
                header::HeaderName::from_static("x-request-id"),
                "req_upstream_1",
            ),
            (
                header::HeaderName::from_static("x-ratelimit-remaining-requests"),
                "41",
            ),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        payload.to_string(),
    )
        .into_response()
}

async fn full_json(response: switchyard_upstream::UpstreamResponse) -> Value {
    match response.body {
        UpstreamBody::Full(bytes) => serde_json::from_slice(&bytes).unwrap(),
        UpstreamBody::Stream(_) => panic!("expected a complete body"),
    }
}

#[tokio::test]
async fn json_success_carries_the_right_request_and_filters_response_headers() {
    let addr = serve(Router::new().route("/v1/chat/completions", post(echo))).await;
    let mut client_headers = HeaderMap::new();
    client_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer client-gateway-key"),
    );
    client_headers.insert(header::COOKIE, HeaderValue::from_static("a=b"));
    client_headers.insert("x-request-id", HeaderValue::from_static("client-req"));
    client_headers.insert(
        "openai-organization",
        HeaderValue::from_static("org-client"),
    );

    let response = client()
        .send(
            &openai(addr),
            &GENERATE,
            body(),
            &client_headers,
            timeouts(),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.media_type().as_deref(), Some("application/json"));
    assert_eq!(response.headers["x-request-id"], "req_upstream_1");
    assert_eq!(response.headers["x-ratelimit-remaining-requests"], "41");
    for dropped in [
        "set-cookie",
        "access-control-allow-origin",
        "content-length",
        "transfer-encoding",
    ] {
        assert!(
            response.headers.get(dropped).is_none(),
            "{dropped} was relayed"
        );
    }

    let seen = full_json(response).await;
    assert_eq!(seen["path"], "/v1/chat/completions");
    assert_eq!(seen["body"]["model"], "gpt-test");
    let headers = &seen["headers"];
    assert_eq!(headers["authorization"], format!("Bearer {KEY}"));
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(headers["accept"], "application/json");
    assert_eq!(headers["openai-organization"], "org-client");
    assert!(
        headers["user-agent"]
            .as_str()
            .unwrap()
            .starts_with("switchyard/")
    );
    assert!(headers.get("cookie").is_none());
    assert!(headers.get("x-request-id").is_none());
    // Compressed responses are welcome on non-streaming calls.
    assert!(
        headers["accept-encoding"]
            .as_str()
            .unwrap()
            .contains("gzip")
    );
}

#[tokio::test]
async fn every_provider_kind_reaches_its_endpoint_with_its_credential() {
    let app = Router::new()
        .route("/v1/messages", post(echo))
        .route("/v1/responses/input_tokens", post(echo))
        .route("/v1beta/models/{model_action}", post(echo))
        .route("/v1/embeddings", post(echo));
    let addr = serve(app).await;
    let base = format!("http://{addr}");
    let client = client();
    let none = HeaderMap::new();

    let anthropic = target(
        ProviderKind::Anthropic,
        Protocol::Anthropic,
        &base,
        "claude-test",
    );
    let seen = full_json(
        client
            .send(&anthropic, &GENERATE, body(), &none, timeouts())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(seen["path"], "/v1/messages");
    assert_eq!(seen["headers"]["x-api-key"], KEY);
    assert_eq!(seen["headers"]["anthropic-version"], "2023-06-01");

    let gemini = target(ProviderKind::Gemini, Protocol::Gemini, &base, "gemini-test");
    let seen = full_json(
        client
            .send(&gemini, &GENERATE, body(), &none, timeouts())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(seen["path"], "/v1beta/models/gemini-test:generateContent");
    assert_eq!(seen["headers"]["x-goog-api-key"], KEY);
    assert_eq!(seen["query"], Value::Null);
    let seen = full_json(
        client
            .send(&gemini, &Operation::CountTokens, body(), &none, timeouts())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(seen["path"], "/v1beta/models/gemini-test:countTokens");

    let responses = target(
        ProviderKind::Openai,
        Protocol::OpenaiResponses,
        format!("{base}/v1"),
        "gpt-test",
    );
    let seen = full_json(
        client
            .send(
                &responses,
                &Operation::CountTokens,
                body(),
                &none,
                timeouts(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(seen["path"], "/v1/responses/input_tokens");

    let seen = full_json(
        client
            .send(
                &responses,
                &Operation::raw_post("embeddings"),
                body(),
                &none,
                timeouts(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(seen["path"], "/v1/embeddings");
    assert_eq!(seen["headers"]["authorization"], format!("Bearer {KEY}"));
}

/// State of the SSE test server: lets the test decide when the second chunk
/// is sent and observe what the request looked like.
#[derive(Clone, Default)]
struct Gate {
    release: Arc<Notify>,
    accept: Arc<Mutex<Option<String>>>,
}

async fn gated_sse(State(gate): State<Gate>, headers: HeaderMap) -> Response {
    *gate.accept.lock().unwrap() = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let release = gate.release.clone();
    let stream = async_stream::stream! {
        yield Ok::<_, std::io::Error>(Bytes::from_static(b"data: {\"n\":1}\n\n"));
        release.notified().await;
        yield Ok(Bytes::from_static(b"data: {\"n\":2}\n\n"));
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    };
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

#[tokio::test]
async fn sse_chunks_arrive_incrementally() {
    let gate = Gate::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(gated_sse))
        .with_state(gate.clone());
    let addr = serve(app).await;

    let response = client()
        .send(
            &openai(addr),
            &STREAM,
            body(),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.media_type().as_deref(), Some("text/event-stream"));
    assert_eq!(
        gate.accept.lock().unwrap().as_deref(),
        Some("text/event-stream")
    );
    let UpstreamBody::Stream(mut stream) = response.body else {
        panic!("expected a byte stream");
    };

    // The first event is readable while the server is still holding back
    // the rest: the body is not buffered.
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(&first[..], b"data: {\"n\":1}\n\n");
    let pending = tokio::time::timeout(Duration::from_millis(150), stream.next()).await;
    assert!(
        pending.is_err(),
        "the second chunk arrived before it was sent"
    );

    gate.release.notify_one();
    let mut rest = Vec::new();
    while let Some(chunk) = stream.next().await {
        rest.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(rest, b"data: {\"n\":2}\n\ndata: [DONE]\n\n");
}

async fn slow_first_chunk() -> Response {
    let stream = async_stream::stream! {
        tokio::time::sleep(Duration::from_millis(400)).await;
        yield Ok::<_, std::io::Error>(Bytes::from_static(b"data: late\n\n"));
    };
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

#[tokio::test]
async fn streams_are_not_bound_by_the_request_timeout() {
    let addr = serve(Router::new().route("/v1/chat/completions", post(slow_first_chunk))).await;
    let short = Timeouts {
        connect: Duration::from_secs(5),
        request: Duration::from_millis(100),
    };
    let response = client()
        .send(&openai(addr), &STREAM, body(), &HeaderMap::new(), short)
        .await
        .unwrap();
    let bytes = response.body.collect().await.unwrap();
    assert_eq!(&bytes[..], b"data: late\n\n");
}

async fn broken_stream() -> Response {
    let stream = async_stream::stream! {
        yield Ok(Bytes::from_static(b"data: {\"n\":1}\n\n"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        yield Err(std::io::Error::other("upstream died"));
    };
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

#[tokio::test]
async fn a_stream_that_dies_midway_yields_a_read_error() {
    let addr = serve(Router::new().route("/v1/chat/completions", post(broken_stream))).await;
    let response = client()
        .send(
            &openai(addr),
            &STREAM,
            body(),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap();
    let UpstreamBody::Stream(mut stream) = response.body else {
        panic!("expected a byte stream");
    };
    let mut good = Vec::new();
    let mut failure = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => good.extend_from_slice(&chunk),
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    assert_eq!(good, b"data: {\"n\":1}\n\n");
    let failure = failure.expect("the stream must end with an error");
    assert_eq!(failure.class, FailureClass::Transport);
    assert_eq!(failure.status, 0);
    assert!(
        failure.info.message.starts_with("read: "),
        "{}",
        failure.info.message
    );
}

async fn rate_limited() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::RETRY_AFTER, "7"),
        ],
        json!({"error": {
            "message": "Rate limit reached for gpt-test in organization org-x on requests per min (RPM): Limit 500, Used 500, Requested 1.",
            "type": "requests",
            "param": null,
            "code": "rate_limit_exceeded"
        }})
        .to_string(),
    )
        .into_response()
}

#[tokio::test]
async fn a_429_becomes_a_rate_limit_error_with_the_retry_after_hint() {
    let addr = serve(Router::new().route("/v1/chat/completions", post(rate_limited))).await;
    let client = client();
    for op in [GENERATE, STREAM] {
        let error = client
            .send(&openai(addr), &op, body(), &HeaderMap::new(), timeouts())
            .await
            .unwrap_err();
        assert_eq!(error.status, 429);
        assert_eq!(error.class, FailureClass::RateLimit);
        assert_eq!(error.retry_after_ms, Some(7_000));
        assert_eq!(error.info.code.as_deref(), Some("rate_limit_exceeded"));
        assert_eq!(error.info.error_type.as_deref(), Some("requests"));
        assert!(error.info.message.starts_with("Rate limit reached"));
        assert_eq!(error.content_type.as_deref(), Some("application/json"));
        let raw: Value = serde_json::from_str(error.body.as_deref().unwrap()).unwrap();
        assert_eq!(raw["error"]["code"], "rate_limit_exceeded");
        assert_eq!(error.to_api_error().retry_after_secs, Some(7));
    }
}

async fn html_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        "<html>\n<head><title>500 Internal Server Error</title></head>\n<body><center><h1>500 Internal Server Error</h1></center><hr><center>nginx</center></body>\n</html>",
    )
        .into_response()
}

#[tokio::test]
async fn a_500_html_page_is_a_server_error_with_a_readable_message() {
    let addr = serve(Router::new().route("/v1/chat/completions", post(html_error))).await;
    let error = client()
        .send(
            &openai(addr),
            &GENERATE,
            body(),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 500);
    assert_eq!(error.class, FailureClass::Server);
    assert_eq!(
        error.info.message,
        "upstream returned an HTML page (HTTP 500 Internal Server Error): 500 Internal Server Error"
    );
    assert!(
        error
            .body
            .as_deref()
            .unwrap()
            .contains("<center>nginx</center>")
    );
    assert_eq!(
        error.content_type.as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(error.retry_after_ms, None);
    assert_eq!(error.to_api_error().status, 502);
}

async fn huge_error() -> Response {
    (StatusCode::BAD_GATEWAY, "x".repeat(3 * 1024 * 1024)).into_response()
}

#[tokio::test]
async fn oversized_error_bodies_are_cut_down() {
    let addr = serve(Router::new().route("/v1/chat/completions", post(huge_error))).await;
    let error = client()
        .send(
            &openai(addr),
            &GENERATE,
            body(),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 502);
    assert_eq!(error.body.as_deref().map(str::len), Some(64 * 1024));
}

async fn never_answers() -> Response {
    tokio::time::sleep(Duration::from_secs(30)).await;
    StatusCode::OK.into_response()
}

#[tokio::test]
async fn a_slow_server_hits_the_request_timeout() {
    let addr = serve(Router::new().route("/v1/chat/completions", post(never_answers))).await;
    let short = Timeouts {
        connect: Duration::from_secs(5),
        request: Duration::from_millis(250),
    };
    let started = Instant::now();
    let error = client()
        .send(&openai(addr), &GENERATE, body(), &HeaderMap::new(), short)
        .await
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(error.class, FailureClass::Transport);
    // 408 makes the client-facing error a 504.
    assert_eq!(error.status, 408);
    assert_eq!(error.to_api_error().status, 504);
    assert!(
        error.info.message.starts_with("timeout: "),
        "{}",
        error.info.message
    );
    assert!(
        error.info.message.contains("250 ms"),
        "{}",
        error.info.message
    );
    assert!(
        error.info.message.contains(&addr.to_string()),
        "{}",
        error.info.message
    );
    assert!(error.body.is_none());
}

async fn stalls_after_headers() -> Response {
    let stream = async_stream::stream! {
        yield Ok::<_, std::io::Error>(Bytes::from_static(b"{\"partial\":"));
        tokio::time::sleep(Duration::from_secs(30)).await;
        yield Ok(Bytes::from_static(b"true}"));
    };
    (
        [(header::CONTENT_TYPE, "application/json")],
        Body::from_stream(stream),
    )
        .into_response()
}

#[tokio::test]
async fn the_request_timeout_also_covers_the_body() {
    let addr = serve(Router::new().route("/v1/chat/completions", post(stalls_after_headers))).await;
    let short = Timeouts {
        connect: Duration::from_secs(5),
        request: Duration::from_millis(300),
    };
    let error = client()
        .send(&openai(addr), &GENERATE, body(), &HeaderMap::new(), short)
        .await
        .unwrap_err();
    assert_eq!(error.class, FailureClass::Transport);
    assert_eq!(error.status, 408);
    assert!(
        error.info.message.starts_with("timeout: "),
        "{}",
        error.info.message
    );
}

#[tokio::test]
async fn tls_failures_name_the_tls_phase() {
    // A plain-HTTP server cannot complete a TLS handshake.
    let addr = serve(Router::new().route("/v1/chat/completions", post(echo))).await;
    let mut target = openai(addr);
    target.base_url = format!("https://{addr}/v1");
    let error = client()
        .send(&target, &GENERATE, body(), &HeaderMap::new(), timeouts())
        .await
        .unwrap_err();
    assert_eq!(error.class, FailureClass::Transport);
    assert_eq!(error.status, 0);
    assert!(
        error.info.message.starts_with("tls: "),
        "{}",
        error.info.message
    );
    assert!(
        error.info.message.contains(&addr.to_string()),
        "{}",
        error.info.message
    );
}

#[tokio::test]
async fn connection_refused_is_a_connect_failure_that_leaks_nothing() {
    let addr = dead_addr().await;
    let mut target = openai(addr);
    target.base_url = format!("http://{addr}/v1?api-key=query-secret-123");
    let error = client()
        .send(&target, &GENERATE, body(), &HeaderMap::new(), timeouts())
        .await
        .unwrap_err();
    assert_eq!(error.class, FailureClass::Transport);
    assert_eq!(error.status, 0);
    let message = &error.info.message;
    assert!(message.starts_with("connect: "), "{message}");
    assert!(message.contains(&addr.to_string()), "{message}");
    assert!(!message.contains(KEY), "{message}");
    assert!(!message.contains("query-secret-123"), "{message}");
    assert!(!message.contains("api-key"), "{message}");
    assert_eq!(error.to_api_error().status, 502);
}

async fn gzipped(headers: HeaderMap) -> Response {
    let accepts_gzip = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("gzip"));
    let payload = json!({"compressed": true, "client_accepts_gzip": accepts_gzip, "padding": "p".repeat(2000)});
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(payload.to_string().as_bytes()).unwrap();
    let compressed = encoder.finish().unwrap();
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CONTENT_ENCODING, "gzip"),
        ],
        compressed,
    )
        .into_response()
}

#[tokio::test]
async fn gzip_responses_are_decoded() {
    let addr = serve(Router::new().route("/v1/chat/completions", post(gzipped))).await;
    let response = client()
        .send(
            &openai(addr),
            &GENERATE,
            body(),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap();
    assert!(response.headers.get(header::CONTENT_ENCODING).is_none());
    assert!(response.headers.get(header::CONTENT_LENGTH).is_none());
    let seen = full_json(response).await;
    assert_eq!(seen["compressed"], true);
    assert_eq!(seen["client_accepts_gzip"], true);
    assert_eq!(seen["padding"].as_str().unwrap().len(), 2000);
}

async fn redirect() -> Response {
    (
        StatusCode::FOUND,
        [(header::LOCATION, "http://attacker.invalid/steal")],
        "",
    )
        .into_response()
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let addr = serve(Router::new().route("/v1/chat/completions", post(redirect))).await;
    let error = client()
        .send(
            &openai(addr),
            &GENERATE,
            body(),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 302);
    assert_eq!(error.class, FailureClass::Server);
}

#[tokio::test]
async fn get_requests_have_no_body() {
    async fn models(headers: HeaderMap, body: Bytes) -> Response {
        axum::Json(json!({
            "has_content_type": headers.contains_key(header::CONTENT_TYPE),
            "body_len": body.len(),
        }))
        .into_response()
    }
    let addr = serve(Router::new().route("/v1/models", get(models))).await;
    let response = client()
        .send(
            &openai(addr),
            &Operation::ListModels,
            Bytes::new(),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap();
    let seen = full_json(response).await;
    assert_eq!(seen, json!({"has_content_type": false, "body_len": 0}));
}

#[tokio::test]
async fn the_mock_kind_has_no_network_endpoint() {
    let target = target(
        ProviderKind::Mock,
        Protocol::OpenaiChat,
        "mock://local",
        "mock-echo",
    );
    let error = client()
        .send(&target, &GENERATE, body(), &HeaderMap::new(), timeouts())
        .await
        .unwrap_err();
    assert_eq!(error.status, 0);
    assert!(
        error.info.message.contains("mock"),
        "{}",
        error.info.message
    );
}

/// One request as the stand-in proxy saw it.
#[derive(Clone, Debug, PartialEq)]
struct Proxied {
    uri: String,
    proxy_authorization: Option<String>,
}

/// What the stand-in proxy saw.
#[derive(Clone, Default)]
struct ProxyLog(Arc<Mutex<Vec<Proxied>>>);

/// An HTTP forward proxy receives absolute-form requests for plain-HTTP
/// destinations; answering them directly is enough to prove the request
/// went through it.
async fn fake_proxy(State(log): State<ProxyLog>, uri: Uri, headers: HeaderMap) -> Response {
    let proxy_authorization = headers
        .get(header::PROXY_AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    log.0.lock().unwrap().push(Proxied {
        uri: uri.to_string(),
        proxy_authorization,
    });
    axum::Json(json!({"via": "proxy"})).into_response()
}

#[tokio::test]
async fn the_most_specific_proxy_setting_is_used() {
    let log = ProxyLog::default();
    let proxy_addr = serve(
        Router::new()
            .fallback(any(fake_proxy))
            .with_state(log.clone()),
    )
    .await;
    let client = client();

    // Credential-level proxy beats provider and global settings.
    let mut via_proxy = target(
        ProviderKind::OpenaiCompat,
        Protocol::OpenaiChat,
        "http://upstream.invalid/v1",
        "m",
    );
    via_proxy.proxy = resolve_proxy(
        &format!("http://user:pass@{proxy_addr}"),
        "http://provider-proxy.invalid:3128",
        "http://global-proxy.invalid:3128",
    );
    assert_eq!(
        via_proxy.proxy,
        ProxySetting::Url(format!("http://user:pass@{proxy_addr}"))
    );
    let response = client
        .send(&via_proxy, &GENERATE, body(), &HeaderMap::new(), timeouts())
        .await
        .unwrap();
    assert_eq!(full_json(response).await, json!({"via": "proxy"}));
    {
        let seen = log.0.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].uri, "http://upstream.invalid/v1/chat/completions");
        // base64("user:pass")
        assert_eq!(
            seen[0].proxy_authorization.as_deref(),
            Some("Basic dXNlcjpwYXNz")
        );
    }

    // `direct` at the credential level bypasses a global proxy entirely.
    let upstream = serve(Router::new().route("/v1/chat/completions", post(echo))).await;
    let mut direct = openai(upstream);
    direct.proxy = resolve_proxy("direct", "", &format!("http://{proxy_addr}"));
    assert_eq!(direct.proxy, ProxySetting::Direct);
    let response = client
        .send(&direct, &GENERATE, body(), &HeaderMap::new(), timeouts())
        .await
        .unwrap();
    assert_eq!(full_json(response).await["path"], "/v1/chat/completions");
    assert_eq!(
        log.0.lock().unwrap().len(),
        1,
        "the direct call must not touch the proxy"
    );

    // With nothing more specific, the global proxy applies.
    let mut global = via_proxy.clone();
    global.proxy = resolve_proxy("", "", &format!("http://{proxy_addr}"));
    client
        .send(&global, &GENERATE, body(), &HeaderMap::new(), timeouts())
        .await
        .unwrap();
    let seen = log.0.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].proxy_authorization, None);
}

#[tokio::test]
async fn an_unreachable_proxy_is_named_without_its_credentials() {
    let dead = dead_addr().await;
    let mut target = target(
        ProviderKind::OpenaiCompat,
        Protocol::OpenaiChat,
        "http://upstream.invalid/v1",
        "m",
    );
    target.proxy = ProxySetting::Url(format!("http://user:hunter2@{dead}"));
    let error = client()
        .send(&target, &GENERATE, body(), &HeaderMap::new(), timeouts())
        .await
        .unwrap_err();
    assert_eq!(error.class, FailureClass::Transport);
    let message = &error.info.message;
    assert!(message.starts_with("connect: "), "{message}");
    assert!(
        message.contains(&format!("via proxy http://redacted@{dead}")),
        "{message}"
    );
    assert!(!message.contains("hunter2"), "{message}");
}

#[tokio::test]
async fn configured_headers_reach_the_upstream() {
    let addr = serve(Router::new().route("/v1/chat/completions", post(echo))).await;
    let mut target = openai(addr);
    target.headers = vec![
        ("X-Team".into(), "blue".into()),
        ("OpenAI-Organization".into(), "org-config".into()),
        ("Authorization".into(), "Bearer not-this-one".into()),
    ];
    let mut client_headers = HeaderMap::new();
    client_headers.insert(
        "openai-organization",
        HeaderValue::from_static("org-client"),
    );
    let seen = full_json(
        client()
            .send(&target, &GENERATE, body(), &client_headers, timeouts())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(seen["headers"]["x-team"], "blue");
    assert_eq!(seen["headers"]["openai-organization"], "org-config");
    assert_eq!(seen["headers"]["authorization"], format!("Bearer {KEY}"));
}

/// "No limit" spelled as the largest duration must behave like no limit on
/// every path: success, stream, error response and failed connect.
#[tokio::test]
async fn unbounded_timeouts_never_panic() {
    let gate = Gate::default();
    gate.release.notify_one();
    let app = Router::new()
        .route("/v1/chat/completions", post(echo))
        .route("/v1/stream/chat/completions", post(gated_sse))
        .route("/v1/busy/chat/completions", post(rate_limited))
        .with_state(gate);
    let addr = serve(app).await;
    let forever = Timeouts {
        connect: Duration::MAX,
        request: Duration::MAX,
    };
    let dead = dead_addr().await;

    let outcome = tokio::spawn(async move {
        let client = client();
        let none = HeaderMap::new();
        let ok = client
            .send(&openai(addr), &GENERATE, body(), &none, forever)
            .await
            .unwrap();
        assert_eq!(ok.status, 200);

        let mut streaming = openai(addr);
        streaming.base_url = format!("http://{addr}/v1/stream");
        let stream = client
            .send(&streaming, &STREAM, body(), &none, forever)
            .await
            .unwrap();
        assert!(
            stream
                .body
                .collect()
                .await
                .unwrap()
                .ends_with(b"[DONE]\n\n")
        );

        let mut busy = openai(addr);
        busy.base_url = format!("http://{addr}/v1/busy");
        let error = client
            .send(&busy, &GENERATE, body(), &none, forever)
            .await
            .unwrap_err();
        assert_eq!(error.status, 429);

        let mut unreachable = openai(addr);
        unreachable.base_url = format!("http://{dead}/v1");
        let error = client
            .send(&unreachable, &GENERATE, body(), &none, forever)
            .await
            .unwrap_err();
        assert_eq!(error.class, FailureClass::Transport);
        assert!(error.info.message.starts_with("connect: "), "{error}");
    })
    .await;
    outcome.expect("send() panicked with unbounded timeouts");
}

async fn proxy_wants_credentials() -> Response {
    (
        StatusCode::PROXY_AUTHENTICATION_REQUIRED,
        [(header::CONTENT_TYPE, "text/html")],
        "<html><head><title>407 Proxy Authentication Required</title></head></html>",
    )
        .into_response()
}

/// Statuses that say nothing about the client's request fail over: only
/// 400, 409, 413 and 422 are request faults by status alone.
#[tokio::test]
async fn an_unlisted_4xx_fails_over_instead_of_blaming_the_request() {
    let addr =
        serve(Router::new().route("/v1/chat/completions", post(proxy_wants_credentials))).await;
    let error = client()
        .send(
            &openai(addr),
            &GENERATE,
            body(),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 407);
    assert_eq!(error.class, FailureClass::Server);
    assert!(error.class.should_failover());
    assert!(
        error
            .info
            .message
            .contains("407 Proxy Authentication Required"),
        "{}",
        error.info.message
    );
}

/// A credential in a configured header is scrubbed like the API key, also
/// from a plain-text answer.
#[tokio::test]
async fn configured_credential_headers_are_scrubbed_from_error_bodies() {
    async fn quoting(headers: HeaderMap) -> Response {
        let seen = headers
            .get("helicone-auth")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        (
            StatusCode::FORBIDDEN,
            format!("rejected gateway credential `{seen}`"),
        )
            .into_response()
    }
    let addr = serve(Router::new().route("/v1/chat/completions", post(quoting))).await;
    let mut target = openai(addr);
    target.headers = vec![(
        "Helicone-Auth".into(),
        "Bearer sk-helicone-0123456789abcdef".into(),
    )];
    let error = client()
        .send(&target, &GENERATE, body(), &HeaderMap::new(), timeouts())
        .await
        .unwrap_err();
    assert_eq!(error.status, 403);
    assert_eq!(
        error.info.message,
        "rejected gateway credential `[redacted]`"
    );
    assert_eq!(
        error.body.as_deref(),
        Some("rejected gateway credential `[redacted]`")
    );
}
