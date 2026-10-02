//! `POST /playground`: one request through the real pipeline, made by the
//! dashboard's built-in client.
//!
//! The answer is exactly what a client of the chosen protocol would get
//! from the client API: the protocol's JSON (success or error, with its own
//! status), or its server-sent events. Only failures of the playground
//! envelope itself use the admin error shape.

use super::{parse_json, read_body};
use crate::Shared;
use crate::auth::AuthContext;
use crate::error::{ApiFailure, ApiResult};
use axum::Extension;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use bytes::Bytes;
use http::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderName, HeaderValue};
use http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use std::convert::Infallible;
use std::time::Duration;
use switchyard_core::{Config, Protocol, sse};
use switchyard_gateway::{ClientRequest, FullReply, Reply, StreamReply};
use tokio_util::sync::{CancellationToken, DropGuard};

/// The endpoint label on the request record.
const ENDPOINT: &str = "POST /admin/api/playground";

/// Request headers handed to the pipeline. The admin secret
/// (`Authorization`, `x-admin-secret`) and cookies are deliberately not
/// among them: nothing downstream should ever see them.
const PASSED_HEADERS: [&str; 6] = [
    "user-agent",
    "anthropic-beta",
    "anthropic-version",
    "openai-beta",
    "x-session-id",
    "session_id",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaygroundRequest {
    protocol: Protocol,
    body: Value,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    stream: Option<bool>,
}

fn body_limit(config: &Config) -> usize {
    usize::try_from(config.server.body_limit_mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX)
}

fn passed_headers(headers: &HeaderMap) -> HeaderMap {
    let mut passed = HeaderMap::new();
    for name in PASSED_HEADERS {
        if let Some(value) = headers.get(name) {
            passed.insert(HeaderName::from_static(name), value.clone());
        }
    }
    passed
}

/// What the pipeline is asked: the body to send and, for Gemini, the two
/// things that protocol puts in the URL.
#[derive(Debug, PartialEq)]
struct Prepared {
    protocol: Protocol,
    body: Value,
    path_model: Option<String>,
    path_stream: Option<bool>,
}

/// Applies `model` and `stream` of the envelope to the protocol body.
///
/// Gemini names the model and the streaming method in the URL, so both
/// stay outside the body and `model` is required. The other protocols carry
/// them in the body: when the envelope gives one, the body is made to
/// agree.
fn prepare(request: PlaygroundRequest) -> Result<Prepared, ApiFailure> {
    let Value::Object(mut body) = request.body else {
        return Err(ApiFailure::bad_field(
            "body",
            "must be a JSON object: the request body of the chosen protocol",
        ));
    };
    let model = request
        .model
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty());
    let (path_model, path_stream) = match request.protocol {
        Protocol::Gemini => {
            let Some(model) = model else {
                return Err(ApiFailure::bad_field(
                    "model",
                    "is required for the gemini protocol, which names the model in the URL",
                ));
            };
            let model = model.strip_prefix("models/").unwrap_or(&model).to_string();
            (Some(model), Some(request.stream.unwrap_or(false)))
        }
        _ => {
            if let Some(model) = model {
                body.insert("model".to_string(), Value::String(model));
            }
            if let Some(stream) = request.stream {
                body.insert("stream".to_string(), Value::Bool(stream));
            }
            (None, None)
        }
    };
    Ok(Prepared {
        protocol: request.protocol,
        body: Value::Object(body),
        path_model,
        path_stream,
    })
}

pub(crate) async fn playground(
    State(state): State<Shared>,
    Extension(context): Extension<AuthContext>,
    request: Request,
) -> ApiResult {
    let config = state.gateway.config();
    let headers = passed_headers(request.headers());
    let bytes = read_body(request, body_limit(&config)).await?;
    let prepared = prepare(parse_json(&bytes)?)?;
    let body = serde_json::to_vec(&prepared.body)
        .map_err(|_| ApiFailure::internal("the request body could not be serialised"))?;

    // Fires when this handler is dropped (the client went away while the
    // upstream was still being called) and, for a stream, when the response
    // body is dropped: the upstream call is abandoned either way.
    let cancel = CancellationToken::new();
    let guard = cancel.clone().drop_guard();

    let mut client = ClientRequest::new(
        prepared.protocol,
        ENDPOINT,
        body,
        state.gateway.dashboard_identity(),
    );
    client.path_model = prepared.path_model;
    client.path_stream = prepared.path_stream;
    client.headers = headers;
    client.client_ip = context.peer.ip.map(|ip| ip.to_string());
    client.cancel = cancel;

    Ok(match state.gateway.generate(client).await {
        Reply::Full(full) => full_response(full),
        Reply::Stream(stream) => {
            let keepalive = Duration::from_secs(config.streaming.keepalive_secs);
            stream_response(stream, guard, keepalive)
        }
    })
}

fn add_headers(target: &mut HeaderMap, pairs: &[(String, String)]) {
    for (name, value) in pairs {
        // The gateway only produces valid pairs; one that is not is left
        // out rather than failing the whole reply.
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            target.append(name, value);
        }
    }
}

/// A complete reply: the gateway's status, content type, headers and body.
fn full_response(full: FullReply) -> Response {
    let mut response = Response::new(Body::from(full.body));
    *response.status_mut() =
        StatusCode::from_u16(full.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let headers = response.headers_mut();
    if let Ok(content_type) = HeaderValue::from_str(&full.content_type) {
        headers.insert(CONTENT_TYPE, content_type);
    }
    add_headers(headers, &full.headers);
    response
}

/// A stream as server-sent events, the way the client API sends them: one
/// chunk per event, a `: keep-alive` comment after `keepalive` of silence
/// (zero disables), and nothing added at the end — the gateway has already
/// rendered the protocol's own terminator.
fn stream_response(stream: StreamReply, guard: DropGuard, keepalive: Duration) -> Response {
    let StreamReply {
        headers: extra,
        events,
        ..
    } = stream;
    let chunks = futures::stream::unfold((events, guard), move |(mut events, guard)| async move {
        let chunk: Option<Bytes> = if keepalive.is_zero() {
            events.recv().await.map(|event| event.to_bytes())
        } else {
            match tokio::time::timeout(keepalive, events.recv()).await {
                Ok(event) => event.map(|event| event.to_bytes()),
                Err(_) => Some(sse::comment("keep-alive")),
            }
        };
        chunk.map(|bytes| (Ok::<Bytes, Infallible>(bytes), (events, guard)))
    });
    let mut response = Response::new(Body::from_stream(chunks));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    add_headers(headers, &extra);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn envelope(value: Value) -> PlaygroundRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn body_protocols_take_model_and_stream_into_the_body() {
        let prepared = prepare(envelope(json!({
            "protocol": "openai-chat",
            "body": {"model": "old", "stream": false, "messages": []},
            "model": " mock-echo ",
            "stream": true,
        })))
        .unwrap();
        assert_eq!(
            prepared.body,
            json!({"model": "mock-echo", "stream": true, "messages": []})
        );
        assert_eq!((prepared.path_model, prepared.path_stream), (None, None));

        // Nothing given: the body is sent as written.
        let untouched = json!({"model": "mock-echo", "max_tokens": 5, "messages": []});
        let prepared = prepare(envelope(json!({
            "protocol": "anthropic",
            "body": untouched,
        })))
        .unwrap();
        assert_eq!(prepared.body, untouched);
    }

    #[test]
    fn gemini_takes_model_and_stream_from_the_envelope() {
        let prepared = prepare(envelope(json!({
            "protocol": "gemini",
            "body": {"contents": []},
            "model": "models/mock-echo",
            "stream": true,
        })))
        .unwrap();
        assert_eq!(prepared.path_model.as_deref(), Some("mock-echo"));
        assert_eq!(prepared.path_stream, Some(true));
        assert_eq!(prepared.body, json!({"contents": []}));

        let no_stream = prepare(envelope(json!({
            "protocol": "gemini", "body": {}, "model": "mock-echo",
        })))
        .unwrap();
        assert_eq!(no_stream.path_stream, Some(false));

        let missing = prepare(envelope(json!({"protocol": "gemini", "body": {}}))).unwrap_err();
        assert_eq!(missing.status, StatusCode::BAD_REQUEST);
        assert_eq!(missing.issues[0].path, "model");
    }

    #[test]
    fn the_body_must_be_an_object() {
        let error =
            prepare(envelope(json!({"protocol": "openai-chat", "body": "hi"}))).unwrap_err();
        assert_eq!(error.issues[0].path, "body");
        assert!(
            serde_json::from_value::<PlaygroundRequest>(json!({"protocol": "smoke", "body": {}}))
                .is_err()
        );
    }

    #[test]
    fn the_admin_secret_is_not_passed_on() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer admin-secret"),
        );
        headers.insert("x-admin-secret", HeaderValue::from_static("admin-secret"));
        headers.insert("cookie", HeaderValue::from_static("session=1"));
        headers.insert("user-agent", HeaderValue::from_static("dashboard/1"));
        headers.insert("anthropic-beta", HeaderValue::from_static("tools-2024"));
        let passed = passed_headers(&headers);
        assert_eq!(passed.len(), 2);
        assert_eq!(passed["user-agent"], "dashboard/1");
        assert_eq!(passed["anthropic-beta"], "tools-2024");
    }
}
