//! `POST /playground`: the four protocols, with and without streaming,
//! through the real pipeline.

mod support;

use futures::StreamExt;
use http::{Method, StatusCode};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{App, eventually, read};

const PROMPT: &str = "say it back";

/// The playground envelope for one protocol: a minimal request body of
/// that protocol asking `model` to answer [`PROMPT`].
fn envelope(protocol: &str, model: &str, stream: Option<bool>) -> Value {
    let body = match protocol {
        "openai-chat" => json!({"messages": [{"role": "user", "content": PROMPT}]}),
        "openai-responses" => json!({"input": PROMPT}),
        "anthropic" => json!({
            "max_tokens": 64,
            "messages": [{"role": "user", "content": PROMPT}],
        }),
        "gemini" => json!({"contents": [{"role": "user", "parts": [{"text": PROMPT}]}]}),
        other => panic!("unknown protocol {other}"),
    };
    let mut envelope = json!({"protocol": protocol, "body": body, "model": model});
    if let Some(stream) = stream {
        envelope["stream"] = json!(stream);
    }
    envelope
}

/// The text of a complete response in each protocol's own shape.
fn answer_text(protocol: &str, body: &Value) -> String {
    let text = match protocol {
        "openai-chat" => body["choices"][0]["message"]["content"].as_str(),
        "openai-responses" => body["output"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["type"] == "message"))
            .and_then(|item| item["content"][0]["text"].as_str()),
        "anthropic" => body["content"][0]["text"].as_str(),
        "gemini" => body["candidates"][0]["content"]["parts"][0]["text"].as_str(),
        other => panic!("unknown protocol {other}"),
    };
    text.unwrap_or_else(|| panic!("{protocol}: no text in {body}"))
        .to_string()
}

/// One parsed server-sent event.
#[derive(Debug)]
struct SseEvent {
    event: Option<String>,
    data: String,
}

fn parse_sse(text: &str) -> Vec<SseEvent> {
    text.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .filter(|block| !block.lines().all(|line| line.starts_with(':')))
        .map(|block| {
            let mut event = None;
            let mut data = Vec::new();
            for line in block.lines() {
                if let Some(name) = line.strip_prefix("event: ") {
                    event = Some(name.to_string());
                } else if let Some(payload) = line.strip_prefix("data: ") {
                    data.push(payload);
                }
            }
            SseEvent {
                event,
                data: data.join("\n"),
            }
        })
        .collect()
}

/// The text a stream delivered, pieced together from each protocol's
/// delta events.
fn streamed_text(protocol: &str, events: &[SseEvent]) -> String {
    let mut text = String::new();
    for event in events {
        let Ok(json) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        let piece = match protocol {
            "openai-chat" => json["choices"][0]["delta"]["content"].as_str(),
            "openai-responses" if json["type"] == "response.output_text.delta" => {
                json["delta"].as_str()
            }
            "anthropic" if json["type"] == "content_block_delta" => json["delta"]["text"].as_str(),
            "gemini" => json["candidates"][0]["content"]["parts"][0]["text"].as_str(),
            _ => None,
        };
        text.push_str(piece.unwrap_or_default());
    }
    text
}

const PROTOCOLS: [&str; 4] = ["openai-chat", "openai-responses", "anthropic", "gemini"];

#[tokio::test]
async fn every_protocol_answers_in_its_own_shape() {
    let app = App::start().await;
    for protocol in PROTOCOLS {
        for stream in [None, Some(false)] {
            let response = app
                .request(Method::POST, "/playground")
                .json(&envelope(protocol, "mock-echo", stream))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{protocol}");
            let headers = response.headers().clone();
            assert!(
                headers["content-type"]
                    .to_str()
                    .unwrap()
                    .starts_with("application/json"),
                "{protocol}: {headers:?}"
            );
            assert!(headers.contains_key("x-request-id"), "{protocol}");
            assert_eq!(headers["x-switchyard-provider"], "mock", "{protocol}");
            let body: Value = response.json().await.unwrap();
            assert!(
                answer_text(protocol, &body).contains(PROMPT),
                "{protocol}: {body}"
            );
        }
    }

    // Each run is a request record made by the built-in dashboard client.
    let page = app.get_ok("/requests").await;
    assert_eq!(page["total"], 8);
    let record = &page["items"][0];
    assert_eq!(record["endpoint"], "POST /admin/api/playground");
    assert_eq!(record["client"]["key_name"], "dashboard");
    assert_eq!(record["client"]["ip"], "127.0.0.1");
    assert_eq!(record["client_protocol"], "gemini");
    assert_eq!(record["transport"], "http");
}

#[tokio::test]
async fn every_protocol_streams_server_sent_events() {
    let app = App::start().await;
    for protocol in PROTOCOLS {
        let response = app
            .request(Method::POST, "/playground")
            .json(&envelope(protocol, "mock-echo", Some(true)))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{protocol}");
        let headers = response.headers().clone();
        assert_eq!(headers["content-type"], "text/event-stream", "{protocol}");
        assert_eq!(headers["cache-control"], "no-cache", "{protocol}");
        assert_eq!(headers["x-accel-buffering"], "no", "{protocol}");
        assert!(headers.contains_key("x-request-id"), "{protocol}");

        let text = response.text().await.unwrap();
        let events = parse_sse(&text);
        assert!(events.len() >= 2, "{protocol}: {text}");
        assert!(
            streamed_text(protocol, &events).contains(PROMPT),
            "{protocol}: {text}"
        );
        // The protocol's own framing, untouched.
        match protocol {
            "openai-chat" => {
                assert_eq!(events.last().unwrap().data, "[DONE]");
                assert!(events.iter().all(|event| event.event.is_none()));
            }
            "openai-responses" => {
                assert_eq!(events[0].event.as_deref(), Some("response.created"));
                assert!(
                    events
                        .iter()
                        .any(|e| e.event.as_deref() == Some("response.completed"))
                );
            }
            "anthropic" => {
                assert_eq!(events[0].event.as_deref(), Some("message_start"));
                assert_eq!(
                    events.last().unwrap().event.as_deref(),
                    Some("message_stop")
                );
            }
            "gemini" => {
                assert!(events.iter().all(|event| event.event.is_none()));
                let last: Value = serde_json::from_str(&events.last().unwrap().data).unwrap();
                assert!(last["candidates"][0]["finishReason"].is_string(), "{last}");
            }
            _ => unreachable!(),
        }
    }
    let page = app.get_ok("/requests").await;
    assert_eq!(page["total"], 4);
    assert_eq!(page["items"][0]["transport"], "sse");
    assert_eq!(page["items"][0]["stream"], true);
}

#[tokio::test]
async fn the_envelope_decides_model_and_stream() {
    let app = App::start().await;

    // `stream` in the envelope overrides the body, in both directions.
    let mut chat = envelope("openai-chat", "mock-echo", Some(false));
    chat["body"]["stream"] = json!(true);
    chat["body"]["model"] = json!("some-other-model");
    let response = app
        .request(Method::POST, "/playground")
        .json(&chat)
        .send()
        .await
        .unwrap();
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "mock-echo");

    // Without `model` and `stream` the body speaks for itself.
    let plain = json!({
        "protocol": "anthropic",
        "body": {
            "model": "mock-echo",
            "max_tokens": 32,
            "stream": true,
            "messages": [{"role": "user", "content": PROMPT}],
        },
    });
    let response = app
        .request(Method::POST, "/playground")
        .json(&plain)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let events = parse_sse(&response.text().await.unwrap());
    assert!(streamed_text("anthropic", &events).contains(PROMPT));
}

#[tokio::test]
async fn failures_come_back_in_the_protocols_error_shape() {
    let app = App::start().await;

    // The pipeline's own errors keep their status and envelope.
    let (status, body) = app
        .post(
            "/playground",
            envelope("openai-chat", "no-such-model", None),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no-such-model"),
        "{body}"
    );
    assert!(body["error"]["type"].is_string(), "{body}");
    let (status, body) = app
        .post("/playground", envelope("anthropic", "no-such-model", None))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["type"], "error", "{body}");
    let (status, body) = app
        .post(
            "/playground",
            envelope("gemini", "no-such-model", Some(true)),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], 404, "{body}");

    // An upstream failure, also for a stream that never started.
    let (status, body) = app
        .post(
            "/playground",
            envelope("openai-chat", "mock-error-429", Some(true)),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(body["error"].is_object(), "{body}");

    // A broken envelope is the admin API's own 400.
    for (envelope, path) in [
        (
            json!({"protocol": "gemini", "body": {"contents": []}}),
            Some("model"),
        ),
        (
            json!({"protocol": "openai-chat", "body": [1, 2, 3]}),
            Some("body"),
        ),
        (json!({"protocol": "morse", "body": {}}), Some("protocol")),
        (json!({"body": {}}), None),
        (
            json!({"protocol": "openai-chat", "body": {}, "temperature": 1}),
            None,
        ),
    ] {
        let (status, body) = app.post("/playground", envelope.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{envelope}: {body}");
        assert!(body["error"]["message"].is_string(), "{body}");
        if let Some(path) = path {
            assert_eq!(
                body["error"]["issues"][0]["path"], path,
                "{envelope}: {body}"
            );
        }
    }
    let (status, _) = read(
        app.request(Method::POST, "/playground")
            .header("content-type", "application/json")
            .body("{"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Larger than `server.body_limit_mb`.
    let (status, body) = app
        .patch("/settings", json!({"server": {"body_limit_mb": 1}}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut huge = envelope("openai-chat", "mock-echo", None);
    huge["body"]["messages"][0]["content"] = json!("x".repeat(1024 * 1024 + 10));
    let (status, body) = app.post("/playground", huge).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert!(body["error"]["message"].is_string(), "{body}");
}

#[tokio::test]
async fn a_client_that_leaves_cancels_the_stream() {
    let app = App::start().await;
    let response = app
        .request(Method::POST, "/playground")
        .json(&envelope("openai-chat", "mock-slow", Some(true)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Read the first chunk, then hang up while the model is still talking.
    let mut body = response.bytes_stream();
    let first = body.next().await.expect("a first chunk").expect("readable");
    assert!(first.starts_with(b"data: "), "{first:?}");
    drop(body);

    // The request is recorded as abandoned, not as a success.
    let telemetry = app.gateway.telemetry().clone();
    let record = eventually(|| {
        telemetry
            .usage()
            .requests(&Default::default())
            .items
            .first()
            .cloned()
    })
    .await;
    assert_eq!(record.requested_model, "mock-slow");
    assert_eq!(record.status, 499, "{record:?}");
    assert_eq!(app.gateway.telemetry().gauges().active_streams(), 0);
}

#[tokio::test]
async fn the_admin_secret_never_reaches_the_request_log() {
    let config = format!("{}\n[logging]\nrequest_log = \"all\"\n", support::BASE);
    let app = App::start_config(&config).await;
    let (status, _) = app
        .post("/playground", envelope("openai-chat", "mock-echo", None))
        .await;
    assert_eq!(status, StatusCode::OK);
    app.gateway.telemetry().flush().await.unwrap();
    let id = app.get_ok("/requests").await["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let detail = app.get_ok(&format!("/requests/{id}")).await;
    assert!(detail["bodies"].is_object(), "{detail}");
    let text = detail.to_string();
    assert!(!text.contains(support::SECRET), "{text}");
    assert!(!text.to_lowercase().contains("authorization"), "{text}");
}
