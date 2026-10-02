//! The HTTP surface of the client API, exercised over real sockets against
//! the built-in mock provider and a small fake upstream.

mod support;

use futures::StreamExt;
use pretty_assertions::assert_eq;
use reqwest::{Response, StatusCode};
use serde_json::{Value, json};
use std::io::Write;
use std::time::Duration;
use support::{KEY, SECOND_KEY, Settings, TestServer, UPSTREAM_KEY, eventually, http, sse_events};

const SAID: &str = "hello there gateway";

fn chat_body(model: &str, stream: bool) -> Value {
    json!({"model": model, "stream": stream,
           "messages": [{"role": "user", "content": SAID}]})
}

fn responses_body(model: &str, stream: bool) -> Value {
    json!({"model": model, "stream": stream,
           "input": [{"role": "user", "content": [{"type": "input_text", "text": SAID}]}]})
}

fn messages_body(model: &str, stream: bool) -> Value {
    json!({"model": model, "stream": stream, "max_tokens": 256,
           "messages": [{"role": "user", "content": SAID}]})
}

fn gemini_body() -> Value {
    json!({"contents": [{"role": "user", "parts": [{"text": SAID}]}]})
}

async fn post(server: &TestServer, path: &str, body: &Value) -> Response {
    http()
        .post(server.url(path))
        .bearer_auth(KEY)
        .json(body)
        .send()
        .await
        .expect("the request must be answered")
}

async fn get(server: &TestServer, path: &str) -> Response {
    http()
        .get(server.url(path))
        .bearer_auth(KEY)
        .send()
        .await
        .expect("the request must be answered")
}

async fn json_of(response: Response) -> Value {
    let text = response.text().await.unwrap();
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("not JSON ({error}): {text}"))
}

fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

fn assert_stamped(response: &Response) {
    assert!(
        header(response, "x-request-id").is_some_and(|id| !id.is_empty()),
        "x-request-id is missing"
    );
    assert_eq!(header(response, "server"), Some("switchyard"));
}

fn assert_sse(response: &Response) {
    assert_eq!(response.status(), 200);
    assert_eq!(header(response, "content-type"), Some("text/event-stream"));
    assert_eq!(header(response, "cache-control"), Some("no-cache"));
    assert_eq!(header(response, "x-accel-buffering"), Some("no"));
    assert_stamped(response);
}

/// Every event of an SSE body is terminated by a blank line and made of
/// `event:` / `data:` lines only.
fn assert_framing(body: &str) {
    assert!(
        body.ends_with("\n\n"),
        "the body must end with a blank line"
    );
    for block in body.trim_end_matches("\n\n").split("\n\n") {
        for line in block.lines() {
            assert!(
                line.starts_with("data: ") || line.starts_with("event: "),
                "unexpected line {line:?} in {block:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Banner and health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_banner_and_health_need_no_key() {
    let server = TestServer::start().await;
    let response = http().get(server.url("/")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_stamped(&response);
    let banner = json_of(response).await;
    assert_eq!(banner["name"], "switchyard");
    assert!(banner["version"].is_string());
    assert!(
        banner["endpoints"]
            .as_array()
            .unwrap()
            .contains(&json!("POST /v1/chat/completions"))
    );

    let response = http().get(server.url("/healthz")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let length = header(&response, "content-length").map(str::to_string);
    assert_eq!(length.as_deref(), Some("15"));
    assert_eq!(json_of(response).await, json!({"status": "ok"}));

    // `HEAD` is `GET` without the body: the same headers, the length too.
    let response = http().head(server.url("/healthz")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_stamped(&response);
    assert_eq!(header(&response, "content-length"), length.as_deref());
    assert_eq!(header(&response, "content-type"), Some("application/json"));
    assert_eq!(response.text().await.unwrap(), "");
}

// ---------------------------------------------------------------------------
// Generation, four protocols, complete and streamed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_completions() {
    let server = TestServer::start().await;
    let response = post(
        &server,
        "/v1/chat/completions",
        &chat_body("mock-echo", false),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_stamped(&response);
    assert_eq!(header(&response, "content-type"), Some("application/json"));
    assert_eq!(header(&response, "x-switchyard-provider"), Some("mock"));
    assert_eq!(header(&response, "x-switchyard-model"), Some("mock-echo"));
    let request_id = header(&response, "x-request-id").unwrap().to_string();
    let body = json_of(response).await;
    assert_eq!(body["object"], "chat.completion");
    let answer = body["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(answer.contains(SAID), "{answer}");

    // The id on the response is the id of the request record.
    let record = server.record(&request_id).await;
    assert_eq!(record.endpoint, "POST /v1/chat/completions");
    assert_eq!(record.status, 200);
    assert_eq!(record.client.key_name.as_deref(), Some("tester"));
    assert_eq!(record.client.ip.as_deref(), Some("127.0.0.1"));
}

#[tokio::test]
async fn chat_completions_stream_ends_with_done() {
    let server = TestServer::start().await;
    let response = post(
        &server,
        "/v1/chat/completions",
        &chat_body("mock-echo", true),
    )
    .await;
    assert_sse(&response);
    let body = response.text().await.unwrap();
    assert_framing(&body);
    assert!(body.ends_with("data: [DONE]\n\n"), "{body}");
    let events = sse_events(&body);
    assert!(events.iter().all(|(name, _)| name.is_none()), "{events:?}");
    let text: String = events
        .iter()
        .filter(|(_, data)| data != "[DONE]")
        .map(|(_, data)| serde_json::from_str::<Value>(data).unwrap())
        .filter_map(|chunk| {
            assert_eq!(chunk["object"], "chat.completion.chunk");
            chunk["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    assert!(text.contains(SAID), "{text}");
    assert_eq!(
        events.iter().filter(|(_, data)| data == "[DONE]").count(),
        1
    );
}

#[tokio::test]
async fn responses() {
    let server = TestServer::start().await;
    let response = post(
        &server,
        "/v1/responses",
        &responses_body("mock-echo", false),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_stamped(&response);
    let body = json_of(response).await;
    assert_eq!(body["object"], "response");
    assert_eq!(body["status"], "completed");
    assert!(body["output"].to_string().contains(SAID), "{body}");

    let response = post(&server, "/v1/responses", &responses_body("mock-echo", true)).await;
    assert_sse(&response);
    let body = response.text().await.unwrap();
    assert_framing(&body);
    assert!(!body.contains("[DONE]"), "Responses streams have no [DONE]");
    let events = sse_events(&body);
    // Every event is named after its type.
    for (name, data) in &events {
        let payload: Value = serde_json::from_str(data).unwrap();
        assert_eq!(name.as_deref(), payload["type"].as_str(), "{data}");
    }
    assert_eq!(
        events.first().unwrap().0.as_deref(),
        Some("response.created")
    );
    assert_eq!(
        events.last().unwrap().0.as_deref(),
        Some("response.completed")
    );
    assert!(
        body.starts_with("event: response.created\ndata: {"),
        "{body}"
    );
}

#[tokio::test]
async fn messages() {
    let server = TestServer::start().await;
    let response = post(&server, "/v1/messages", &messages_body("mock-echo", false)).await;
    assert_eq!(response.status(), 200);
    assert_stamped(&response);
    let body = json_of(response).await;
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert!(body["content"][0]["text"].as_str().unwrap().contains(SAID));

    let response = post(&server, "/v1/messages", &messages_body("mock-echo", true)).await;
    assert_sse(&response);
    let body = response.text().await.unwrap();
    assert_framing(&body);
    assert!(!body.contains("[DONE]"));
    let events = sse_events(&body);
    let names: Vec<&str> = events
        .iter()
        .map(|(name, _)| name.as_deref().expect("Anthropic events are named"))
        .collect();
    assert_eq!(names.first(), Some(&"message_start"));
    assert_eq!(names.last(), Some(&"message_stop"));
    assert!(names.contains(&"content_block_delta"));
    assert!(body.starts_with("event: message_start\ndata: {"), "{body}");
}

#[tokio::test]
async fn gemini_generate_content() {
    let server = TestServer::start().await;
    let response = post(
        &server,
        "/v1beta/models/mock-echo:generateContent",
        &gemini_body(),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_stamped(&response);
    let request_id = header(&response, "x-request-id").unwrap().to_string();
    let body = json_of(response).await;
    let text = body["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains(SAID), "{text}");
    let record = server.record(&request_id).await;
    assert_eq!(
        record.endpoint,
        "POST /v1beta/models/{model}:generateContent"
    );
    assert_eq!(record.requested_model, "mock-echo");

    // The same route for Vertex-style clients.
    let response = post(
        &server,
        "/v1/models/mock-echo:generateContent",
        &gemini_body(),
    )
    .await;
    assert_eq!(response.status(), 200);
    let body = json_of(response).await;
    assert!(body["candidates"][0]["content"]["parts"][0]["text"].is_string());

    // The key in the query, as Google's clients send it.
    let response = http()
        .post(server.url(&format!(
            "/v1beta/models/mock-echo:generateContent?key={KEY}"
        )))
        .json(&gemini_body())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn gemini_streams_sse_only_when_asked_and_a_json_array_otherwise() {
    let server = TestServer::start().await;
    let response = post(
        &server,
        "/v1beta/models/mock-echo:streamGenerateContent?alt=sse",
        &gemini_body(),
    )
    .await;
    assert_sse(&response);
    let body = response.text().await.unwrap();
    assert_framing(&body);
    assert!(!body.contains("[DONE]"));
    let events = sse_events(&body);
    assert!(events.len() > 1);
    let mut sse_text = String::new();
    for (name, data) in &events {
        assert_eq!(*name, None, "Gemini events carry data only");
        let chunk: Value = serde_json::from_str(data).unwrap();
        if let Some(text) = chunk["candidates"][0]["content"]["parts"][0]["text"].as_str() {
            sse_text.push_str(text);
        }
    }
    assert!(sse_text.contains(SAID), "{sse_text}");

    // Without `alt=sse`: one JSON array, like Google's own API.
    let response = post(
        &server,
        "/v1beta/models/mock-echo:streamGenerateContent",
        &gemini_body(),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "content-type"), Some("application/json"));
    assert_stamped(&response);
    let body = response.text().await.unwrap();
    assert!(body.starts_with('[') && body.ends_with(']'), "{body}");
    assert!(!body.contains("data: ") && !body.contains("keep-alive"));
    let chunks: Vec<Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(chunks.len(), events.len());
    let array_text: String = chunks
        .iter()
        .filter_map(|chunk| chunk["candidates"][0]["content"]["parts"][0]["text"].as_str())
        .collect();
    assert_eq!(array_text, sse_text);
}

#[tokio::test]
async fn gemini_unknown_methods_are_404_in_googles_shape() {
    let server = TestServer::start().await;
    for path in [
        "/v1beta/models/mock-echo:embedContent",
        "/v1beta/models/mock-echo",
        "/v1/models/mock-echo:predict",
    ] {
        let response = post(&server, path, &gemini_body()).await;
        assert_eq!(response.status(), 404, "{path}");
        assert_stamped(&response);
        let body = json_of(response).await;
        assert_eq!(body["error"]["code"], 404, "{path}");
        assert_eq!(body["error"]["status"], "NOT_FOUND", "{path}");
    }
}

#[tokio::test]
async fn legacy_completions_are_served_through_chat() {
    let server = TestServer::start().await;
    let legacy = json!({"model": "mock-echo", "prompt": SAID, "max_tokens": 64});
    let response = post(&server, "/v1/completions", &legacy).await;
    assert_eq!(response.status(), 200);
    let request_id = header(&response, "x-request-id").unwrap().to_string();
    let body = json_of(response).await;
    assert_eq!(body["object"], "text_completion");
    assert!(body["choices"][0]["text"].as_str().unwrap().contains(SAID));
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert!(body["choices"][0].get("message").is_none());
    assert_eq!(
        server.record(&request_id).await.endpoint,
        "POST /v1/completions"
    );

    let mut streamed = legacy.clone();
    streamed["stream"] = json!(true);
    let response = post(&server, "/v1/completions", &streamed).await;
    assert_sse(&response);
    let body = response.text().await.unwrap();
    assert_framing(&body);
    assert!(body.ends_with("data: [DONE]\n\n"));
    let mut text = String::new();
    for (_, data) in sse_events(&body) {
        if data == "[DONE]" {
            continue;
        }
        let chunk: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(chunk["object"], "text_completion", "{data}");
        assert!(chunk["choices"][0].get("delta").is_none(), "{data}");
        text.push_str(chunk["choices"][0]["text"].as_str().unwrap_or(""));
    }
    assert!(text.contains(SAID), "{text}");

    // Errors come back as the pipeline wrote them.
    let response = post(
        &server,
        "/v1/completions",
        &json!({"model": "no-such-model", "prompt": "x"}),
    )
    .await;
    assert_eq!(response.status(), 404);
    let body = json_of(response).await;
    assert_eq!(body["error"]["code"], "model_not_found");
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

#[tokio::test]
async fn model_listings_in_three_shapes() {
    let server = TestServer::start().await;

    let response = get(&server, "/v1/models").await;
    assert_eq!(response.status(), 200);
    assert_stamped(&response);
    let openai = json_of(response).await;
    assert_eq!(openai["object"], "list");
    let ids: Vec<&str> = openai["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| {
            assert_eq!(model["object"], "model");
            model["id"].as_str().unwrap()
        })
        .collect();
    for expected in ["mock-echo", "mock-slow", "embed", "recorded"] {
        assert!(
            ids.contains(&expected),
            "{expected} is missing from {ids:?}"
        );
    }

    let response = http()
        .get(server.url("/v1/models"))
        .header("x-api-key", KEY)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let anthropic = json_of(response).await;
    assert_eq!(anthropic["has_more"], false);
    assert!(anthropic.get("object").is_none());
    assert!(anthropic["first_id"].is_string());
    let entry = anthropic["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["id"] == "mock-echo")
        .expect("mock-echo is listed");
    assert_eq!(entry["type"], "model");
    assert!(entry["display_name"].is_string());

    let response = get(&server, "/v1beta/models").await;
    assert_eq!(response.status(), 200);
    let gemini = json_of(response).await;
    let names: Vec<&str> = gemini["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"models/mock-echo"), "{names:?}");

    // A key restricted to some models sees only those.
    let response = http()
        .get(server.url("/v1/models"))
        .bearer_auth(SECOND_KEY)
        .send()
        .await
        .unwrap();
    let restricted = json_of(response).await;
    let ids: Vec<&str> = restricted["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"mock-echo"));
    assert!(!ids.contains(&"embed"), "{ids:?}");
}

#[tokio::test]
async fn single_models_in_three_shapes() {
    let server = TestServer::start().await;

    let model = json_of(get(&server, "/v1/models/mock-echo").await).await;
    assert_eq!(model["id"], "mock-echo");
    assert_eq!(model["object"], "model");

    let response = http()
        .get(server.url("/v1/models/mock-echo"))
        .bearer_auth(KEY)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .unwrap();
    let model = json_of(response).await;
    assert_eq!(model["id"], "mock-echo");
    assert_eq!(model["type"], "model");

    for path in [
        "/v1beta/models/mock-echo",
        "/v1beta/models/models/mock-echo",
    ] {
        let model = json_of(get(&server, path).await).await;
        assert_eq!(model["name"], "models/mock-echo", "{path}");
    }

    let response = get(&server, "/v1/models/nope").await;
    assert_eq!(response.status(), 404);
    assert_eq!(json_of(response).await["error"]["code"], "model_not_found");

    let response = http()
        .get(server.url("/v1/models/nope"))
        .bearer_auth(KEY)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let body = json_of(response).await;
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "not_found_error");

    let response = get(&server, "/v1beta/models/nope").await;
    assert_eq!(response.status(), 404);
    assert_eq!(json_of(response).await["error"]["status"], "NOT_FOUND");
}

// ---------------------------------------------------------------------------
// Token counting and raw endpoints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn count_endpoints() {
    let server = TestServer::start().await;

    let response = post(
        &server,
        "/v1/messages/count_tokens",
        &json!({"model": "mock-echo", "messages": [{"role": "user", "content": SAID}]}),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_stamped(&response);
    let request_id = header(&response, "x-request-id").unwrap().to_string();
    let count = json_of(response).await;
    assert!(count["input_tokens"].as_u64().unwrap() > 0, "{count}");
    assert_eq!(
        server.record(&request_id).await.endpoint,
        "POST /v1/messages/count_tokens"
    );

    let response = post(
        &server,
        "/v1/responses/input_tokens",
        &json!({"model": "mock-echo", "input": SAID}),
    )
    .await;
    assert_eq!(response.status(), 200);
    let count = json_of(response).await;
    assert!(count["input_tokens"].as_u64().unwrap() > 0, "{count}");

    let response = post(
        &server,
        "/v1beta/models/mock-echo:countTokens",
        &gemini_body(),
    )
    .await;
    assert_eq!(response.status(), 200);
    let count = json_of(response).await;
    assert!(count["totalTokens"].as_u64().unwrap() > 0, "{count}");
}

#[tokio::test]
async fn embeddings_are_proxied_to_the_upstream_that_serves_the_model() {
    let server = TestServer::start().await;
    let response = http()
        .post(server.url(&format!("/v1/embeddings?key={KEY}&encoding_format=float")))
        .json(&json!({"model": "embed", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_stamped(&response);
    assert_eq!(header(&response, "x-switchyard-provider"), Some("fake"));
    assert_eq!(
        header(&response, "x-ratelimit-remaining-requests"),
        Some("41")
    );
    let request_id = header(&response, "x-request-id").unwrap().to_string();
    let body = json_of(response).await;
    assert_eq!(body["data"][0]["embedding"], json!([0.25, -0.5]));

    let seen = server.fake.on("/v1/embeddings");
    assert_eq!(seen.len(), 1);
    // The upstream gets its own model id and the gateway's credential …
    assert_eq!(seen[0].body, json!({"model": "embed-up", "input": "hi"}));
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some(format!("Bearer {UPSTREAM_KEY}").as_str())
    );
    // … and the client's query without the client's key.
    assert_eq!(seen[0].query, "encoding_format=float");
    assert_eq!(
        server.record(&request_id).await.endpoint,
        "POST /v1/embeddings"
    );

    // No model, no route.
    for body in [
        json!({"input": "hi"}),
        json!("just a string"),
        json!({"model": " "}),
    ] {
        let response = post(&server, "/v1/embeddings", &body).await;
        assert_eq!(response.status(), 400, "{body}");
        let error = json_of(response).await;
        assert_eq!(error["error"]["param"], "model");
    }
    // A model no OpenAI-style upstream serves.
    let response = post(
        &server,
        "/v1/moderations",
        &json!({"model": "mock-echo", "input": "hi"}),
    )
    .await;
    assert_eq!(response.status(), 404);
    assert!(json_of(response).await["error"]["message"].is_string());
    assert_eq!(server.fake.on("/v1/embeddings").len(), 1);
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_and_wrong_keys_are_401_in_each_protocols_shape() {
    let server = TestServer::start().await;
    let client = http();
    for (path, body) in [
        ("/v1/chat/completions", chat_body("mock-echo", false)),
        ("/v1/responses", responses_body("mock-echo", false)),
        ("/v1/embeddings", json!({"model": "embed", "input": "x"})),
    ] {
        let response = client
            .post(server.url(path))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{path}");
        assert_stamped(&response);
        let error = json_of(response).await;
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("missing API key"),
            "{error}"
        );
        assert_eq!(error["error"]["type"], "authentication_error", "{path}");
    }
    let response = client.get(server.url("/v1/models")).send().await.unwrap();
    assert_eq!(response.status(), 401);
    let error = json_of(response).await;
    assert!(error["error"]["message"].is_string());
    assert!(error.get("type").is_none(), "OpenAI's envelope: {error}");
    // The model routes are shared with Anthropic's API: a client that
    // announces itself as one is refused in that shape.
    for path in ["/v1/models", "/v1/models/mock-echo"] {
        let response = client
            .get(server.url(path))
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", "sy-not-a-key")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{path}");
        let error = json_of(response).await;
        assert_eq!(error["type"], "error", "{path}");
        assert_eq!(error["error"]["type"], "authentication_error", "{path}");
    }

    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        let response = client
            .post(server.url(path))
            .json(&messages_body("mock-echo", false))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{path}");
        let error = json_of(response).await;
        assert_eq!(error["type"], "error", "{path}");
        assert_eq!(error["error"]["type"], "authentication_error", "{path}");
    }

    let response = client
        .post(server.url("/v1beta/models/mock-echo:generateContent"))
        .json(&gemini_body())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let error = json_of(response).await;
    assert_eq!(error["error"]["code"], 401);
    assert_eq!(error["error"]["status"], "UNAUTHENTICATED");
    let response = client
        .get(server.url("/v1beta/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(
        json_of(response).await["error"]["status"],
        "UNAUTHENTICATED"
    );

    // A key that is not configured.
    let response = client
        .post(server.url("/v1/chat/completions"))
        .bearer_auth("sy-not-a-key")
        .json(&chat_body("mock-echo", false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let error = json_of(response).await;
    let message = error["error"]["message"].as_str().unwrap();
    assert!(message.contains("invalid API key"), "{message}");
    assert!(!message.contains("sy-not-a-key"));
}

#[tokio::test]
async fn the_key_is_accepted_in_every_documented_place() {
    let server = TestServer::start().await;
    let client = http();
    let url = server.url("/v1/chat/completions");
    let body = chat_body("mock-echo", false);

    let attempts = [
        client
            .post(&url)
            .header("authorization", format!("Bearer {KEY}")),
        client
            .post(&url)
            .header("authorization", format!("bearer {KEY}")),
        // The raw header value, without a scheme.
        client.post(&url).header("authorization", KEY),
        client.post(&url).header("x-api-key", KEY),
        client.post(&url).header("x-goog-api-key", KEY),
        client.post(format!("{url}?key={KEY}")),
        // The first candidate that matches wins: a wrong one before it does
        // not spoil a right one.
        client
            .post(&url)
            .header("authorization", "Bearer sy-wrong")
            .header("x-api-key", KEY),
        client
            .post(format!("{url}?key={KEY}"))
            .header("x-api-key", "sy-wrong")
            .header("x-goog-api-key", "sy-wrong-too"),
    ];
    for (index, attempt) in attempts.into_iter().enumerate() {
        let response = attempt.json(&body).send().await.unwrap();
        assert_eq!(response.status(), 200, "attempt {index}");
    }

    // The record names the key that matched.
    let response = client
        .post(&url)
        .header("authorization", format!("Bearer {SECOND_KEY}"))
        .header("x-api-key", KEY)
        .json(&body)
        .send()
        .await
        .unwrap();
    let request_id = header(&response, "x-request-id").unwrap().to_string();
    assert_eq!(
        server.record(&request_id).await.client.key_name.as_deref(),
        Some("second")
    );
}

#[tokio::test]
async fn anonymous_clients_are_admitted_when_auth_is_not_required() {
    let server = TestServer::with(Settings {
        auth_required: false,
        ..Settings::default()
    })
    .await;
    let client = http();
    let response = client
        .post(server.url("/v1/chat/completions"))
        .json(&chat_body("mock-echo", false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    // Even with a key nobody configured.
    let response = client
        .get(server.url("/v1/models"))
        .bearer_auth("whatever")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cors_preflights_are_answered_before_authentication() {
    let server = TestServer::start().await;
    let client = http();
    for path in [
        "/v1/chat/completions",
        "/v1/messages",
        "/v1beta/models",
        "/anything",
    ] {
        let response = client
            .request(reqwest::Method::OPTIONS, server.url(path))
            .header("origin", "https://app.example")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "authorization, x-custom")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "{path}");
        assert_stamped(&response);
        assert_eq!(header(&response, "access-control-allow-origin"), Some("*"));
        assert_eq!(
            header(&response, "access-control-allow-methods"),
            Some("GET, POST, PUT, PATCH, DELETE, OPTIONS")
        );
        assert_eq!(
            header(&response, "access-control-allow-headers"),
            Some("authorization, x-custom")
        );
        assert_eq!(
            header(&response, "access-control-expose-headers"),
            Some("x-request-id, retry-after, x-switchyard-provider, x-switchyard-model")
        );
        assert_eq!(header(&response, "access-control-max-age"), Some("600"));
        assert_eq!(response.text().await.unwrap(), "");
    }

    // Without requested headers, any header is allowed.
    let response = client
        .request(reqwest::Method::OPTIONS, server.url("/v1/responses"))
        .send()
        .await
        .unwrap();
    assert_eq!(header(&response, "access-control-allow-headers"), Some("*"));

    // Real responses carry the headers too — errors included.
    let response = post(
        &server,
        "/v1/chat/completions",
        &chat_body("mock-echo", false),
    )
    .await;
    assert_eq!(header(&response, "access-control-allow-origin"), Some("*"));
    assert!(header(&response, "access-control-expose-headers").is_some());
    let response = client
        .post(server.url("/v1/chat/completions"))
        .json(&chat_body("mock-echo", false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(header(&response, "access-control-allow-origin"), Some("*"));
    let response = post(
        &server,
        "/v1/chat/completions",
        &chat_body("mock-echo", true),
    )
    .await;
    assert_eq!(header(&response, "access-control-allow-origin"), Some("*"));
}

#[tokio::test]
async fn cors_can_be_switched_off() {
    let server = TestServer::with(Settings {
        cors: false,
        ..Settings::default()
    })
    .await;
    let response = http()
        .request(reqwest::Method::OPTIONS, server.url("/v1/chat/completions"))
        .header("access-control-request-method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 405);
    assert_eq!(header(&response, "allow"), Some("POST"));
    assert!(header(&response, "access-control-allow-origin").is_none());

    let response = post(
        &server,
        "/v1/chat/completions",
        &chat_body("mock-echo", false),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_stamped(&response);
    for name in [
        "access-control-allow-origin",
        "access-control-allow-methods",
        "access-control-allow-headers",
        "access-control-expose-headers",
        "access-control-max-age",
    ] {
        assert!(header(&response, name).is_none(), "{name}");
    }
}

/// With CORS off a browser keeps other sites' pages from *reading* answers;
/// it still sends their "simple" requests. Where nothing but the network
/// position admits a client, the gateway has to refuse those itself, or a
/// page could run requests it never sees the answers to.
#[tokio::test]
async fn with_cors_off_requests_from_other_sites_pages_are_refused() {
    let server = TestServer::with(Settings {
        cors: false,
        auth_required: false,
        ..Settings::default()
    })
    .await;
    let client = http();
    // What a page can send without a preflight: a POST labelled as text.
    let from_page = |path: &str, body: &Value| {
        client
            .post(server.url(path))
            .header("content-type", "text/plain")
            .body(body.to_string())
    };

    for (path, body) in [
        ("/v1/chat/completions", chat_body("mock-echo", false)),
        ("/v1/messages", messages_body("mock-echo", false)),
        ("/v1beta/models/mock-echo:generateContent", gemini_body()),
        ("/v1/embeddings", json!({"model": "embed", "input": "x"})),
    ] {
        let response = from_page(path, &body)
            .header("origin", "https://evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{path}");
        assert_stamped(&response);
        assert!(header(&response, "access-control-allow-origin").is_none());
        let error = json_of(response).await;
        let message = error["error"]["message"].as_str().unwrap_or("");
        assert!(message.contains("other origins"), "{path}: {error}");
        // In the shape of the route's API family.
        match path {
            "/v1/messages" => assert_eq!(error["error"]["type"], "permission_error"),
            "/v1beta/models/mock-echo:generateContent" => {
                assert_eq!(error["error"]["status"], "PERMISSION_DENIED")
            }
            _ => assert_eq!(error["error"]["code"], "origin_not_allowed", "{path}"),
        }
    }
    // The browser's own word for it, whatever `Origin` says.
    let response = from_page("/v1/chat/completions", &chat_body("mock-echo", false))
        .header("origin", format!("http://{}", server.addr))
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    // Nothing ran, nothing reached an upstream.
    assert!(server.records().is_empty());
    assert!(server.fake.recorded().is_empty());

    // A page of the gateway's own origin is served …
    let response = from_page("/v1/chat/completions", &chat_body("mock-echo", false))
        .header("origin", format!("http://{}", server.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    // … also behind a proxy that rewrote `Host`, when the browser vouches
    // for it …
    let response = from_page("/v1/chat/completions", &chat_body("mock-echo", false))
        .header("origin", "https://gateway.example")
        .header("sec-fetch-site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    // … and the model listings, which run nothing, are not guarded (the
    // page cannot read them without CORS headers anyway).
    let response = client
        .get(server.url("/v1/models"))
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(header(&response, "access-control-allow-origin").is_none());

    // With CORS on, every origin is welcome: that is what the switch is for.
    let open = TestServer::with(Settings {
        cors: true,
        auth_required: false,
        ..Settings::default()
    })
    .await;
    let response = client
        .post(open.url("/v1/chat/completions"))
        .header("origin", "https://app.example")
        .header("sec-fetch-site", "cross-site")
        .json(&chat_body("mock-echo", false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "access-control-allow-origin"), Some("*"));
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn brotli(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut writer = brotli::CompressorWriter::new(&mut out, 4096, 5, 20);
        writer.write_all(data).unwrap();
    }
    out
}

fn zstd(data: &[u8]) -> Vec<u8> {
    zstd::stream::encode_all(data, 3).unwrap()
}

async fn post_encoded(server: &TestServer, path: &str, encoding: &str, body: Vec<u8>) -> Response {
    http()
        .post(server.url(path))
        .bearer_auth(KEY)
        .header("content-type", "application/json")
        .header("content-encoding", encoding)
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn compressed_request_bodies_are_decoded() {
    let server = TestServer::start().await;
    let plain = chat_body("mock-echo", false).to_string().into_bytes();
    for (encoding, body) in [
        ("gzip", gzip(&plain)),
        ("deflate", zlib(&plain)),
        ("br", brotli(&plain)),
        ("zstd", zstd(&plain)),
        ("gzip, br", brotli(&gzip(&plain))),
        ("identity", plain.clone()),
    ] {
        let response = post_encoded(&server, "/v1/chat/completions", encoding, body).await;
        assert_eq!(response.status(), 200, "{encoding}");
        let answer = json_of(response).await;
        assert!(
            answer["choices"][0]["message"]["content"]
                .as_str()
                .unwrap()
                .contains(SAID),
            "{encoding}"
        );
    }

    // Every route decodes, not only the OpenAI ones.
    let plain = messages_body("mock-echo", false).to_string().into_bytes();
    let response = post_encoded(&server, "/v1/messages", "zstd", zstd(&plain)).await;
    assert_eq!(response.status(), 200);
    let response = post_encoded(
        &server,
        "/v1beta/models/mock-echo:generateContent",
        "gzip",
        gzip(gemini_body().to_string().as_bytes()),
    )
    .await;
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn unknown_content_encodings_are_415() {
    let server = TestServer::start().await;
    let plain = chat_body("mock-echo", false).to_string().into_bytes();
    let response = post_encoded(&server, "/v1/chat/completions", "compress", plain.clone()).await;
    assert_eq!(response.status(), 415);
    assert_stamped(&response);
    let error = json_of(response).await;
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("compress")
    );

    let response = post_encoded(&server, "/v1/messages", "lz4", plain).await;
    assert_eq!(response.status(), 415);
    assert_eq!(json_of(response).await["type"], "error");
}

#[tokio::test]
async fn oversized_and_exploding_bodies_are_413_in_each_protocols_shape() {
    // The limit is 1 MiB.
    let server = TestServer::start().await;
    let padding = "x".repeat(1024 * 1024 + 512);

    let mut big = chat_body("mock-echo", false);
    big["padding"] = json!(padding);
    let response = post(&server, "/v1/chat/completions", &big).await;
    assert_eq!(response.status(), 413);
    assert_stamped(&response);
    let error = json_of(response).await;
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("1 MiB")
    );

    let mut big = messages_body("mock-echo", false);
    big["padding"] = json!(padding);
    let response = post(&server, "/v1/messages", &big).await;
    assert_eq!(response.status(), 413);
    let error = json_of(response).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["type"], "request_too_large");

    let mut big = gemini_body();
    big["padding"] = json!(padding);
    let response = post(&server, "/v1beta/models/mock-echo:generateContent", &big).await;
    assert_eq!(response.status(), 413);
    assert_eq!(json_of(response).await["error"]["code"], 413);

    // Small on the wire, 8 MiB once decompressed.
    let mut bomb = chat_body("mock-echo", false);
    bomb["padding"] = json!("0".repeat(8 * 1024 * 1024));
    let bomb = bomb.to_string().into_bytes();
    for (encoding, body) in [
        ("gzip", gzip(&bomb)),
        ("br", brotli(&bomb)),
        ("zstd", zstd(&bomb)),
        ("deflate", zlib(&bomb)),
    ] {
        assert!(body.len() < 256 * 1024, "{encoding}: {}", body.len());
        let response = post_encoded(&server, "/v1/chat/completions", encoding, body).await;
        assert_eq!(response.status(), 413, "{encoding}");
    }

    // Just under the limit still works.
    let mut fits = chat_body("mock-echo", false);
    fits["padding"] = json!("x".repeat(900 * 1024));
    let response = post(&server, "/v1/chat/completions", &fits).await;
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn refusals_reach_clients_that_are_still_sending_their_body() {
    let server = TestServer::start().await;
    let client = http();
    let mut big = chat_body("mock-echo", false);
    big["padding"] = json!("x".repeat(2 * 1024 * 1024));

    // Each of these is decided before the body is read. The client must
    // get the answer, not a connection reset under its upload.
    for _ in 0..2 {
        let response = client
            .post(server.url("/v1/chat/completions"))
            .bearer_auth("sy-wrong-key")
            .json(&big)
            .send()
            .await
            .expect("a 401, not a reset");
        assert_eq!(response.status(), 401);

        let response = client
            .post(server.url("/v1/no/such/route"))
            .bearer_auth(KEY)
            .json(&big)
            .send()
            .await
            .expect("a 404, not a reset");
        assert_eq!(response.status(), 404);

        let response = client
            .put(server.url("/v1/chat/completions"))
            .bearer_auth(KEY)
            .json(&big)
            .send()
            .await
            .expect("a 405, not a reset");
        assert_eq!(response.status(), 405);

        let response = post(&server, "/v1/chat/completions", &big).await;
        assert_eq!(response.status(), 413);

        let response = client
            .post(server.url("/v1/chat/completions"))
            .bearer_auth(KEY)
            .header("content-encoding", "lz4")
            .json(&big)
            .send()
            .await
            .expect("a 415, not a reset");
        assert_eq!(response.status(), 415);
    }
}

#[tokio::test]
async fn empty_and_broken_bodies_are_the_pipelines_400() {
    let server = TestServer::start().await;
    let client = http();
    for (path, shape) in [
        ("/v1/chat/completions", "openai"),
        ("/v1/messages", "anthropic"),
        ("/v1beta/models/mock-echo:generateContent", "google"),
    ] {
        for body in ["", "{not json"] {
            let response = client
                .post(server.url(path))
                .bearer_auth(KEY)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 400, "{path} {body:?}");
            assert_stamped(&response);
            let error = json_of(response).await;
            match shape {
                "openai" => assert!(error["error"]["message"].is_string()),
                "anthropic" => assert_eq!(error["error"]["type"], "invalid_request_error"),
                _ => assert_eq!(error["error"]["status"], "INVALID_ARGUMENT"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming behaviour
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_keep_alive_comment_is_sent_during_silence() {
    let server = TestServer::with(Settings {
        keepalive_secs: 1,
        ..Settings::default()
    })
    .await;
    // The upstream goes quiet for longer than the keep-alive interval.
    server.fake.pause_chat(Duration::from_millis(1300));
    let response = post(
        &server,
        "/v1/chat/completions",
        &chat_body("paused-chat", true),
    )
    .await;
    assert_sse(&response);
    let body = response.text().await.unwrap();
    assert!(body.ends_with("data: [DONE]\n\n"), "{body}");
    assert_eq!(body.matches(": keep-alive\n\n").count(), 1, "{body}");
    // The comment sits in the gap, between the two text chunks.
    let comment = body.find(": keep-alive\n\n").unwrap();
    let first = body.find("\"Hel\"").unwrap();
    let second = body.find("\"lo\"").unwrap();
    assert!(first < comment && comment < second, "{body}");
    // And it is invisible to an SSE parser.
    let text: String = sse_events(&body)
        .iter()
        .filter(|(_, data)| data != "[DONE]")
        .filter_map(|(_, data)| {
            serde_json::from_str::<Value>(data).unwrap()["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    assert_eq!(text, "Hello");
}

#[tokio::test]
async fn no_keep_alive_when_disabled_or_when_events_keep_coming() {
    let server = TestServer::start().await;
    server.fake.pause_chat(Duration::from_millis(300));
    let response = post(
        &server,
        "/v1/chat/completions",
        &chat_body("paused-chat", true),
    )
    .await;
    let body = response.text().await.unwrap();
    assert!(!body.contains("keep-alive"), "{body}");
}

#[tokio::test]
async fn a_client_that_disconnects_mid_stream_cancels_the_request() {
    let server = TestServer::start().await;
    let long = "word ".repeat(200);
    let body = json!({"model": "mock-slow", "stream": true,
                      "messages": [{"role": "user", "content": long}]});
    let response = post(&server, "/v1/chat/completions", &body).await;
    assert_sse(&response);
    let request_id = header(&response, "x-request-id").unwrap().to_string();
    let gauges = server.gateway.telemetry().gauges().clone();
    assert_eq!(gauges.active_streams(), 1);

    let mut stream = response.bytes_stream();
    let first = stream.next().await.expect("a first chunk").unwrap();
    assert!(first.starts_with(b"data: "));
    // Hang up with most of the answer still to come.
    drop(stream);

    let record = server.record(&request_id).await;
    assert_eq!(record.status, 499);
    assert_eq!(record.error.as_ref().unwrap().kind, "client_disconnect");
    eventually("the gauges to return to zero", || {
        gauges.active_streams() == 0 && gauges.in_flight() == 0
    })
    .await;
}

#[tokio::test]
async fn upstream_failures_keep_their_status_even_for_streams() {
    let server = TestServer::start().await;
    let response = post(
        &server,
        "/v1/chat/completions",
        &chat_body("mock-error-429", true),
    )
    .await;
    assert_eq!(response.status(), 429);
    assert_stamped(&response);
    assert_eq!(header(&response, "content-type"), Some("application/json"));
    assert!(header(&response, "retry-after").is_some());
    assert!(json_of(response).await["error"]["message"].is_string());

    let response = post(
        &server,
        "/v1/messages",
        &messages_body("mock-error-500", false),
    )
    .await;
    assert!(response.status().is_server_error());
    let error = json_of(response).await;
    assert_eq!(error["type"], "error");

    let response = post(
        &server,
        "/v1/chat/completions",
        &chat_body("no-such-model", false),
    )
    .await;
    assert_eq!(response.status(), 404);
    assert_eq!(json_of(response).await["error"]["code"], "model_not_found");
}

// ---------------------------------------------------------------------------
// Unknown routes and methods
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_paths_are_404_and_wrong_methods_405() {
    let server = TestServer::start().await;
    let client = http();

    for path in ["/nope", "/v1", "/v1/chat", "/v1/chat/completions/"] {
        let response = client.get(server.url(path)).send().await.unwrap();
        assert_eq!(response.status(), 404, "{path}");
        assert_stamped(&response);
        assert_eq!(header(&response, "content-type"), Some("application/json"));
        let error = json_of(response).await;
        assert_eq!(error["error"]["type"], "invalid_request_error", "{path}");
        assert!(error["error"]["message"].as_str().unwrap().contains(path));
    }
    let response = client
        .get(server.url("/v1/messages/batches"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let error = json_of(response).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["type"], "not_found_error");
    let response = client
        .get(server.url("/v1beta/files"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(json_of(response).await["error"]["status"], "NOT_FOUND");

    // The query string — where a key may be — is not echoed.
    let response = client
        .get(server.url("/nope?key=sy-secret-in-query"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert!(
        !response
            .text()
            .await
            .unwrap()
            .contains("sy-secret-in-query")
    );

    for (method, path, allow) in [
        (reqwest::Method::GET, "/v1/chat/completions", "POST"),
        (reqwest::Method::DELETE, "/v1/responses", "GET, POST"),
        (reqwest::Method::POST, "/v1/models", "GET"),
        (reqwest::Method::POST, "/healthz", "GET, HEAD"),
        (reqwest::Method::PUT, "/v1/embeddings", "POST"),
    ] {
        let response = client
            .request(method.clone(), server.url(path))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 405, "{method} {path}");
        assert_stamped(&response);
        assert_eq!(header(&response, "allow"), Some(allow), "{method} {path}");
        assert!(json_of(response).await["error"]["message"].is_string());
    }
    let response = client.get(server.url("/v1/messages")).send().await.unwrap();
    assert_eq!(response.status(), 405);
    assert_eq!(json_of(response).await["type"], "error");
    let response = client
        .put(server.url("/v1beta/models/mock-echo"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 405);
    assert_eq!(json_of(response).await["error"]["code"], 405);
}

#[tokio::test]
async fn settings_are_read_from_the_live_configuration() {
    let server = TestServer::start().await;
    let response = post(
        &server,
        "/v1/chat/completions",
        &chat_body("mock-echo", false),
    )
    .await;
    assert_eq!(header(&response, "access-control-allow-origin"), Some("*"));

    // Switch CORS off and raise the body limit while the server runs.
    let mut applied = server.gateway.telemetry().subscribe();
    server
        .gateway
        .config_store()
        .update(|config| {
            config.server.cors = false;
            config.server.body_limit_mb = 3;
            Ok(())
        })
        .await
        .expect("the edit must be valid");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if applied.recv().await.unwrap().topic() == "config.reloaded" {
                break;
            }
        }
    })
    .await
    .expect("the configuration must be applied");

    let mut big = chat_body("mock-echo", false);
    big["padding"] = json!("x".repeat(2 * 1024 * 1024));
    let response = post(&server, "/v1/chat/completions", &big).await;
    assert_eq!(response.status(), 200);
    assert!(header(&response, "access-control-allow-origin").is_none());
}
