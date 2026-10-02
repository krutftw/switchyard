//! The HTTP handlers: banner, health, model listings, generation, token
//! counting and the raw side endpoints.

use crate::app::Context;
use crate::body;
use crate::respond::{self, EventMap, Framing};
use crate::route::{GeminiMethod, RawEndpoint, Route};
use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use http::{HeaderMap, HeaderValue, Method};
use serde_json::{Value, json};
use std::time::Duration;
use switchyard_codecs::chat::{chat_response_to_completions, completions_request_to_chat};
use switchyard_core::{ApiError, Protocol};
use switchyard_gateway::{ClientRequest, Gateway, RawRequest, Reply};
use tokio_util::sync::CancellationToken;

/// `GET /`: what this server is and what it serves.
pub(crate) fn banner() -> Response {
    respond::json(&json!({
        "name": "switchyard",
        "version": Gateway::version(),
        "endpoints": [
            "GET /healthz",
            "GET /v1/models",
            "GET /v1/models/{id}",
            "POST /v1/chat/completions",
            "POST /v1/completions",
            "POST /v1/responses",
            "GET /v1/responses (WebSocket)",
            "POST /v1/responses/input_tokens",
            "POST /v1/messages",
            "POST /v1/messages/count_tokens",
            "GET /v1beta/models",
            "GET /v1beta/models/{name}",
            "POST /v1beta/models/{model}:generateContent",
            "POST /v1beta/models/{model}:streamGenerateContent",
            "POST /v1beta/models/{model}:countTokens",
            "GET /v1/realtime (WebSocket)",
            "POST /v1/embeddings",
            "POST /v1/images/generations",
            "POST /v1/moderations",
            "POST /v1/audio/speech",
        ],
    }))
}

/// `GET`, `HEAD /healthz`.
pub(crate) fn health(method: &Method) -> Response {
    let status = json!({"status": "ok"});
    let mut response = respond::json(&status);
    if *method == Method::HEAD {
        // The headers of the `GET` answer — its length included — without
        // the body.
        response
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from(status.to_string().len()));
        *response.body_mut() = Body::empty();
    }
    response
}

/// The shape `/v1/models` answers in: Anthropic's for clients that send an
/// `anthropic-version` header (every Anthropic SDK does), OpenAI's otherwise.
pub(crate) fn models_protocol(headers: &HeaderMap) -> Protocol {
    let anthropic = headers
        .get("anthropic-version")
        .is_some_and(|value| !value.as_bytes().is_empty());
    if anthropic {
        Protocol::Anthropic
    } else {
        Protocol::OpenaiChat
    }
}

/// `GET /v1/models`
pub(crate) fn models(context: &Context) -> Response {
    let protocol = models_protocol(&context.headers);
    respond::json(&context.gateway().models(protocol, &context.identity))
}

/// `GET /v1/models/{id}`
pub(crate) fn model(context: &Context, id: &str) -> Response {
    let protocol = models_protocol(&context.headers);
    match context.gateway().model(protocol, &context.identity, id) {
        Ok(model) => respond::json(&model),
        Err(error) => respond::error(context.gateway(), protocol, &error),
    }
}

/// `GET /v1beta/models`
pub(crate) fn gemini_models(context: &Context) -> Response {
    respond::json(
        &context
            .gateway()
            .models(Protocol::Gemini, &context.identity),
    )
}

/// `GET /v1beta/models/{name}`
pub(crate) fn gemini_model(context: &Context, name: &str) -> Response {
    match context
        .gateway()
        .model(Protocol::Gemini, &context.identity, name)
    {
        Ok(model) => respond::json(&model),
        Err(error) => context.error(&error),
    }
}

/// Every route that takes a body: reads it (size limit, content decoding)
/// and runs the route.
pub(crate) async fn with_body(context: Context, route: Route, body: Body) -> Response {
    // The limit in force now, not the one at start-up.
    let limit = body::limit_bytes(context.gateway().config().server.body_limit_mb);
    let body = match body::read(body, &context.headers, limit).await {
        Ok(body) => body,
        Err(error) => return context.error(&error.to_api_error()),
    };
    match route {
        Route::ChatCompletions => {
            let job = Job::new(Protocol::OpenaiChat, "POST /v1/chat/completions", body);
            generate(context, job).await
        }
        Route::Completions => completions(context, body).await,
        Route::Responses => {
            let job = Job::new(Protocol::OpenaiResponses, "POST /v1/responses", body);
            generate(context, job).await
        }
        Route::ResponsesInputTokens => {
            let job = Job::new(
                Protocol::OpenaiResponses,
                "POST /v1/responses/input_tokens",
                body,
            );
            count(context, job).await
        }
        Route::Messages => {
            let job = Job::new(Protocol::Anthropic, "POST /v1/messages", body);
            generate(context, job).await
        }
        Route::MessagesCountTokens => {
            let job = Job::new(Protocol::Anthropic, "POST /v1/messages/count_tokens", body);
            count(context, job).await
        }
        Route::GeminiAction {
            model,
            method,
            prefix,
        } => gemini(context, model, method, prefix, body).await,
        Route::Raw(endpoint) => raw(context, endpoint, body).await,
        // Routes without a body never get here; answer like an unknown one.
        Route::Banner
        | Route::Health
        | Route::Models
        | Route::Model(_)
        | Route::GeminiModels
        | Route::GeminiModel(_)
        | Route::ResponsesWebSocket
        | Route::Realtime => context.error(&ApiError::not_found("no such route")),
    }
}

/// One generation or counting request, before it is given to the gateway.
struct Job {
    protocol: Protocol,
    endpoint: String,
    body: Bytes,
    path_model: Option<String>,
    path_stream: Option<bool>,
    framing: StreamShape,
    map: EventMap,
}

/// How a streamed reply is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamShape {
    Sse,
    JsonArray,
}

impl Job {
    fn new(protocol: Protocol, endpoint: impl Into<String>, body: Bytes) -> Job {
        Job {
            protocol,
            endpoint: endpoint.into(),
            body,
            path_model: None,
            path_stream: None,
            framing: StreamShape::Sse,
            map: EventMap::Verbatim,
        }
    }

    /// The gateway request for this job, with a fresh cancellation token.
    fn request(&self, context: &Context, cancel: CancellationToken) -> ClientRequest {
        let mut request = ClientRequest::new(
            self.protocol,
            self.endpoint.clone(),
            self.body.clone(),
            context.identity.clone(),
        );
        request.path_model = self.path_model.clone();
        request.path_stream = self.path_stream;
        request.headers = context.headers.clone();
        request.client_ip = context.client_ip.clone();
        request.cancel = cancel;
        request
    }
}

/// The longest keep-alive interval honoured. Anything above it is treated
/// as this, so that a nonsensical setting cannot overflow timer arithmetic.
const MAX_KEEPALIVE_SECS: u64 = 24 * 60 * 60;

/// `streaming.keepalive_secs` as it is now; `0` switches keep-alives off.
pub(crate) fn keepalive(gateway: &Gateway) -> Option<Duration> {
    match gateway.config().streaming.keepalive_secs {
        0 => None,
        secs => Some(Duration::from_secs(secs.min(MAX_KEEPALIVE_SECS))),
    }
}

/// Runs a generation request and renders the reply.
async fn generate(context: Context, job: Job) -> Response {
    reply_to_response(&context, &job, run(&context, &job).await)
}

/// Calls the pipeline. The returned guard cancels the request when dropped:
/// by hyper dropping this handler's future (the client went away before the
/// reply), or by the response body being dropped mid-stream.
async fn run(context: &Context, job: &Job) -> (Reply, tokio_util::sync::DropGuard) {
    let cancel = CancellationToken::new();
    let guard = cancel.clone().drop_guard();
    let reply = context
        .gateway()
        .generate(job.request(context, cancel))
        .await;
    (reply, guard)
}

fn reply_to_response(
    context: &Context,
    job: &Job,
    (reply, guard): (Reply, tokio_util::sync::DropGuard),
) -> Response {
    match reply {
        Reply::Full(full) => {
            // Answered in full: nothing is left to cancel.
            drop(guard.disarm());
            respond::full(full)
        }
        Reply::Stream(stream) => {
            let framing = match job.framing {
                StreamShape::Sse => Framing::Sse {
                    keepalive: keepalive(context.gateway()),
                },
                StreamShape::JsonArray => Framing::JsonArray,
            };
            respond::stream(stream, framing, job.map, guard)
        }
    }
}

/// Runs a token-counting request.
async fn count(context: Context, job: Job) -> Response {
    let cancel = CancellationToken::new();
    // Fires if this future is dropped while the count is still being made.
    let cancel_on_drop = cancel.clone().drop_guard();
    let reply = context
        .gateway()
        .count_tokens(job.request(&context, cancel))
        .await;
    drop(cancel_on_drop.disarm());
    match reply {
        Reply::Full(full) => respond::full(full),
        Reply::Stream(_) => context.error(&ApiError::internal(
            "token counting produced a stream instead of a count",
        )),
    }
}

/// `POST /v1/completions`: the legacy endpoint, served as a Chat
/// Completions request whose reply is rewritten into the `text_completion`
/// shape.
async fn completions(context: Context, body: Bytes) -> Response {
    let mut job = Job::new(Protocol::OpenaiChat, "POST /v1/completions", body);
    job.map = EventMap::LegacyCompletions;
    // A body that is not JSON goes to the pipeline as it is, which answers
    // with the proper 400.
    if let Ok(legacy) = serde_json::from_slice::<Value>(&job.body)
        && legacy.is_object()
    {
        job.body = Bytes::from(completions_request_to_chat(&legacy).to_string());
    }
    let (reply, guard) = run(&context, &job).await;
    let reply = match reply {
        Reply::Full(mut full) => {
            // Only a chat completion is rewritten; an error envelope keeps
            // its bytes.
            if let Ok(chat) = serde_json::from_slice::<Value>(&full.body)
                && chat.get("choices").is_some_and(Value::is_array)
            {
                full.body = Bytes::from(chat_response_to_completions(&chat).to_string());
            }
            Reply::Full(full)
        }
        stream => stream,
    };
    reply_to_response(&context, &job, (reply, guard))
}

/// `POST …/models/{model}:{method}`
async fn gemini(
    context: Context,
    model: String,
    method: GeminiMethod,
    prefix: &'static str,
    body: Bytes,
) -> Response {
    let endpoint = format!("POST {prefix}/models/{{model}}:{}", method.as_str());
    let mut job = Job::new(Protocol::Gemini, endpoint, body);
    job.path_model = Some(model);
    match method {
        GeminiMethod::Count => count(context, job).await,
        GeminiMethod::Generate => {
            job.path_stream = Some(false);
            generate(context, job).await
        }
        GeminiMethod::Stream => {
            job.path_stream = Some(true);
            // Google streams server-sent events only when asked to with
            // `alt=sse`; otherwise the chunks are the elements of one JSON
            // array.
            let alt = context
                .query
                .get("alt")
                .or_else(|| context.query.get("$alt"));
            if !alt.is_some_and(|alt| alt.eq_ignore_ascii_case("sse")) {
                job.framing = StreamShape::JsonArray;
            }
            generate(context, job).await
        }
    }
}

/// The side endpoints proxied as raw JSON to an OpenAI-style upstream
/// chosen by the body's `model`.
async fn raw(context: Context, endpoint: RawEndpoint, body: Bytes) -> Response {
    let model = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_string)
        });
    let Some(model) = model else {
        return context.error(
            &ApiError::invalid_request(
                "the request body must be a JSON object with a `model` field",
            )
            .with_param("model"),
        );
    };
    let cancel = CancellationToken::new();
    // Fires if this future is dropped while the upstream is still working.
    let cancel_on_drop = cancel.clone().drop_guard();
    let request = RawRequest {
        path: endpoint.upstream_path().to_string(),
        method: Method::POST,
        body,
        content_type: context
            .headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        query: context.query.without(&[]),
        model,
        headers: context.headers.clone(),
        identity: context.identity.clone(),
        client_ip: context.client_ip.clone(),
        endpoint: endpoint.endpoint().to_string(),
        cancel,
    };
    let reply = context.gateway().raw(request).await;
    drop(cancel_on_drop.disarm());
    match reply {
        Reply::Full(full) => respond::full(full),
        Reply::Stream(_) => context.error(&ApiError::internal(
            "a raw endpoint produced a stream instead of a response",
        )),
    }
}
