//! Responses over WebSocket (`GET /v1/responses`), against the mock
//! provider and a recording fake upstream.

mod support;

use futures::SinkExt;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::time::Duration;
use support::{KEY, Received, Settings, TestServer, WsClient, eventually, http, types, ws_connect};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

const DONE: &[&str] = &["response.completed", "error"];

async fn connect(server: &TestServer) -> WsClient {
    let auth = format!("Bearer {KEY}");
    ws_connect(
        &server.ws_url("/v1/responses"),
        &[("authorization", auth.as_str())],
    )
    .await
    .expect("the upgrade must succeed")
}

fn user(text: &str) -> Value {
    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
}

fn create(model: &str, input: Value) -> Value {
    json!({"type": "response.create", "model": model, "input": input})
}

/// The text a completed response carries.
fn output_text(completed: &Value) -> String {
    completed["response"]["output"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
                .filter_map(|part| part["text"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The texts of the message items of an upstream `input`, and the types of
/// the others.
fn summary(input: &Value) -> Vec<String> {
    input
        .as_array()
        .expect("input is an array")
        .iter()
        .map(|item| match item["type"].as_str() {
            Some("message") | None => {
                let text: String = match &item["content"] {
                    Value::String(text) => text.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect(),
                    _ => String::new(),
                };
                format!("{}:{text}", item["role"].as_str().unwrap_or("?"))
            }
            Some(other) => format!("{other}:{}", item["call_id"].as_str().unwrap_or("")),
        })
        .collect()
}

#[tokio::test]
async fn a_turn_is_one_json_event_per_text_frame() {
    let server = TestServer::start().await;
    let mut client = connect(&server).await;
    let gauges = server.gateway.telemetry().gauges().clone();
    eventually("the connection to be counted", || {
        gauges.ws_connections() == 1
    })
    .await;

    client
        .send_json(&create("mock-echo", json!([user("hello over a socket")])))
        .await;
    let frames = client.read_until(DONE).await;
    let kinds = types(&frames);
    assert_eq!(kinds.first(), Some(&"response.created"));
    assert_eq!(kinds.last(), Some(&"response.completed"));
    assert!(kinds.contains(&"response.output_text.delta"), "{kinds:?}");
    // Bare events: no SSE framing, no terminator, nothing but objects.
    for frame in &frames {
        assert!(frame.is_object(), "{frame}");
        assert!(frame["type"].is_string(), "{frame}");
    }
    let completed = frames.last().unwrap();
    assert!(output_text(completed).contains("hello over a socket"));
    let response_id = completed["response"]["id"].as_str().unwrap().to_string();

    // The connection stays open for the next turn, which builds on this one.
    client
        .send_json(
            &json!({"type": "response.create", "previous_response_id": response_id,
                           "input": [user("and again")]}),
        )
        .await;
    let frames = client.read_until(DONE).await;
    assert!(output_text(frames.last().unwrap()).contains("and again"));

    // Each turn is a request of its own in the records.
    let records = server.records();
    let turns: Vec<_> = records
        .iter()
        .filter(|record| record.endpoint == "WS /v1/responses")
        .collect();
    assert_eq!(turns.len(), 2);
    for turn in turns {
        assert_eq!(turn.status, 200);
        assert_eq!(turn.transport, switchyard_telemetry::Transport::Websocket);
        assert_eq!(turn.client.key_name.as_deref(), Some("tester"));
    }

    client.socket.close(None).await.unwrap();
    assert_eq!(client.read_close().await, Received::Close(None));
    eventually("the connection to be released", || {
        gauges.ws_connections() == 0
    })
    .await;
}

#[tokio::test]
async fn a_follow_up_sends_the_whole_conversation_upstream() {
    let server = TestServer::start().await;
    let mut client = connect(&server).await;

    let mut first = create("recorded", json!([user("one")]));
    first["instructions"] = json!("be brief");
    first["store"] = json!(false);
    client.send_json(&first).await;
    let frames = client.read_until(DONE).await;
    let completed = frames.last().unwrap();
    assert_eq!(completed["type"], "response.completed", "{completed}");
    assert_eq!(output_text(completed), "answer 1");

    client
        .send_json(&json!({
            "type": "response.create",
            "previous_response_id": completed["response"]["id"],
            "input": [user("two")]
        }))
        .await;
    let frames = client.read_until(DONE).await;
    assert_eq!(output_text(frames.last().unwrap()), "answer 2");

    let seen = server.fake.on("/v1/responses");
    assert_eq!(seen.len(), 2);
    let first = &seen[0].body;
    assert_eq!(first["model"], "resp-up");
    assert_eq!(first["stream"], true);
    assert_eq!(first["instructions"], "be brief");
    assert_eq!(summary(&first["input"]), vec!["user:one"]);

    // Turn two: the first input, the first answer, the new input — and
    // nothing of the WebSocket protocol.
    let second = &seen[1].body;
    assert_eq!(
        summary(&second["input"]),
        vec!["user:one", "assistant:answer 1", "user:two"]
    );
    assert_eq!(second["model"], "resp-up", "the model is inherited");
    assert_eq!(
        second["instructions"], "be brief",
        "so are the instructions"
    );
    assert_eq!(second["stream"], true);
    for key in ["type", "previous_response_id", "generate", "stream_id"] {
        assert!(second.get(key).is_none(), "`{key}` reached the upstream");
        assert!(first.get(key).is_none(), "`{key}` reached the upstream");
    }
    // Not inherited: only model and instructions are.
    assert!(second.get("store").is_none());

    // The legacy `response.append` continues the same way.
    client
        .send_json(&json!({"type": "response.append", "input": [user("three")]}))
        .await;
    client.read_until(DONE).await;
    let third = &server.fake.on("/v1/responses")[2].body;
    assert_eq!(
        summary(&third["input"]),
        vec![
            "user:one",
            "assistant:answer 1",
            "user:two",
            "assistant:answer 2",
            "user:three"
        ]
    );
}

#[tokio::test]
async fn replayed_history_replaces_the_transcript() {
    let server = TestServer::start().await;
    let mut client = connect(&server).await;
    client
        .send_json(&create("recorded", json!([user("one")])))
        .await;
    client.read_until(DONE).await;

    // No `previous_response_id`, and the input contains model output: the
    // client is sending its (rewritten) history, not an increment.
    let history = json!([
        user("summary of earlier"),
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "noted"}]},
        user("continue")
    ]);
    client
        .send_json(&json!({"type": "response.create", "input": history}))
        .await;
    let frames = client.read_until(DONE).await;
    assert_eq!(frames.last().unwrap()["type"], "response.completed");

    let seen = server.fake.on("/v1/responses");
    assert_eq!(
        summary(&seen[1].body["input"]),
        vec![
            "user:summary of earlier",
            "assistant:noted",
            "user:continue"
        ]
    );
    assert_eq!(seen[1].body["model"], "resp-up");

    // And the next increment builds on the replacement.
    client
        .send_json(&json!({"type": "response.create",
                           "previous_response_id": frames.last().unwrap()["response"]["id"],
                           "input": [user("more")]}))
        .await;
    client.read_until(DONE).await;
    let seen = server.fake.on("/v1/responses");
    assert_eq!(
        summary(&seen[2].body["input"]),
        vec![
            "user:summary of earlier",
            "assistant:noted",
            "user:continue",
            "assistant:answer 2",
            "user:more"
        ]
    );
}

/// `previous_response_id` alone says what a request continues. Without it a
/// `response.create` is a request of its own — nothing of the conversation
/// before reaches the upstream with it, its instructions included.
#[tokio::test]
async fn a_create_without_previous_response_id_starts_afresh() {
    let server = TestServer::start().await;
    let mut client = connect(&server).await;

    let mut first = create("recorded", json!([user("cat")]));
    first["instructions"] = json!("translate into French");
    client.send_json(&first).await;
    let frames = client.read_until(DONE).await;
    let first_id = frames.last().unwrap()["response"]["id"].clone();

    // The model may be left out (the connection's last one is used); the
    // other request's instructions are not carried over.
    client
        .send_json(&json!({"type": "response.create", "input": [user("capital of Peru?")]}))
        .await;
    let frames = client.read_until(DONE).await;
    assert_eq!(frames.last().unwrap()["type"], "response.completed");
    let seen = server.fake.on("/v1/responses");
    assert_eq!(
        summary(&seen[1].body["input"]),
        vec!["user:capital of Peru?"]
    );
    assert_eq!(seen[1].body["model"], "resp-up");
    assert!(
        seen[1].body.get("instructions").is_none(),
        "the instructions of an unrelated request reached the upstream: {}",
        seen[1].body
    );

    // The lane has moved on: the first response can no longer be continued,
    // and saying so does not cost the connection.
    client
        .send_json(
            &json!({"type": "response.create", "previous_response_id": first_id,
                           "input": [user("and dog?")]}),
        )
        .await;
    let error = client.next_json().await;
    assert_eq!(error["type"], "error", "{error}");
    assert_eq!(error["status"], 409);
    assert_eq!(error["error"]["code"], "previous_response_not_found");
    assert_eq!(server.fake.on("/v1/responses").len(), 2);

    // The client recovers as the protocol says: the full context, no id.
    client
        .send_json(
            &json!({"type": "response.create", "instructions": "translate into French",
                           "input": [user("cat"),
                                     {"type": "message", "role": "assistant",
                                      "content": [{"type": "output_text", "text": "chat"}]},
                                     user("and dog?")]}),
        )
        .await;
    let frames = client.read_until(DONE).await;
    assert_eq!(frames.last().unwrap()["type"], "response.completed");
    let seen = server.fake.on("/v1/responses");
    assert_eq!(
        summary(&seen[2].body["input"]),
        vec!["user:cat", "assistant:chat", "user:and dog?"]
    );
    assert_eq!(seen[2].body["instructions"], "translate into French");
}

/// Each lane (`stream_id`) has a conversation of its own, continued by the
/// id of its latest response whatever happened on other lanes since; and a
/// response can be forked onto a new lane.
#[tokio::test]
async fn lanes_keep_their_conversations_apart() {
    let server = TestServer::start().await;
    let mut client = connect(&server).await;
    let on_lane = |lane: &str, previous: Option<&Value>, text: &str| {
        let mut request = json!({"type": "response.create", "stream_id": lane,
                                 "input": [user(text)]});
        match previous {
            Some(id) => request["previous_response_id"] = id.clone(),
            None => request["model"] = json!("recorded"),
        }
        request
    };

    client.send_json(&on_lane("a", None, "about a")).await;
    let a1 = client.read_until(DONE).await.last().unwrap()["response"]["id"].clone();
    client.send_json(&on_lane("b", None, "about b")).await;
    let b1 = client.read_until(DONE).await.last().unwrap()["response"]["id"].clone();

    // Lane a goes on although b's response is the latest of the connection.
    client.send_json(&on_lane("a", Some(&a1), "more a")).await;
    let frames = client.read_until(DONE).await;
    let completed = frames.last().unwrap();
    assert_eq!(completed["type"], "response.completed", "{completed}");
    assert_eq!(completed["stream_id"], "a");
    // A fork of b's response onto a new lane; b itself stays where it was.
    client.send_json(&on_lane("c", Some(&b1), "what if")).await;
    client.read_until(DONE).await;
    client.send_json(&on_lane("b", Some(&b1), "more b")).await;
    let frames = client.read_until(DONE).await;
    assert_eq!(frames.last().unwrap()["type"], "response.completed");

    let seen = server.fake.on("/v1/responses");
    assert_eq!(seen.len(), 5);
    assert_eq!(
        summary(&seen[2].body["input"]),
        vec!["user:about a", "assistant:answer 1", "user:more a"]
    );
    assert_eq!(
        summary(&seen[3].body["input"]),
        vec!["user:about b", "assistant:answer 2", "user:what if"]
    );
    assert_eq!(
        summary(&seen[4].body["input"]),
        vec!["user:about b", "assistant:answer 2", "user:more b"]
    );

    // Lane a's first response is behind it now.
    client.send_json(&on_lane("a", Some(&a1), "again")).await;
    let error = client.next_json().await;
    assert_eq!(error["status"], 409, "{error}");
    assert_eq!(error["stream_id"], "a");
}

#[tokio::test]
async fn a_prewarm_is_answered_locally_and_continued_from() {
    let server = TestServer::start().await;
    let mut client = connect(&server).await;

    let mut prewarm = create("recorded", json!([user("context")]));
    prewarm["generate"] = json!(false);
    prewarm["instructions"] = json!("sys");
    client.send_json(&prewarm).await;
    let created = client.next_json().await;
    let completed = client.next_json().await;
    assert_eq!(created["type"], "response.created");
    assert_eq!(completed["type"], "response.completed");
    let prewarm_id = completed["response"]["id"].as_str().unwrap().to_string();
    assert!(prewarm_id.starts_with("resp_prewarm_"), "{prewarm_id}");
    assert_eq!(completed["response"]["output"], json!([]));
    assert_eq!(completed["response"]["usage"]["total_tokens"], 0);
    assert_eq!(completed["response"]["model"], "recorded");
    assert!(
        server.fake.on("/v1/responses").is_empty(),
        "a prewarm must not reach the upstream"
    );

    // A different id is unknown here.
    client
        .send_json(
            &json!({"type": "response.create", "previous_response_id": "resp_other",
                           "input": [user("go")]}),
        )
        .await;
    let error = client.next_json().await;
    assert_eq!(error["status"], 409);

    // The prewarm's id continues from the prewarm's input.
    client
        .send_json(
            &json!({"type": "response.create", "previous_response_id": prewarm_id,
                           "input": [user("go")]}),
        )
        .await;
    let frames = client.read_until(DONE).await;
    assert_eq!(frames.last().unwrap()["type"], "response.completed");
    let seen = server.fake.on("/v1/responses");
    assert_eq!(seen.len(), 1);
    assert_eq!(
        summary(&seen[0].body["input"]),
        vec!["user:context", "user:go"]
    );
    assert_eq!(seen[0].body["instructions"], "sys");
    assert!(seen[0].body.get("generate").is_none());
    assert!(seen[0].body.get("previous_response_id").is_none());
}

#[tokio::test]
async fn validation_errors_are_answered_in_band_and_keep_the_socket_open() {
    let server = TestServer::start().await;
    let mut client = connect(&server).await;

    let cases: Vec<(Message, u16, &str)> = vec![
        (
            Message::Text("this is not json".into()),
            400,
            "invalid websocket request: each message must be one JSON object",
        ),
        (
            Message::Text("[1, 2]".into()),
            400,
            "invalid websocket request: each message must be one JSON object",
        ),
        (
            Message::Text(json!({"type": "response.cancel"}).to_string().into()),
            400,
            "unsupported websocket request type: response.cancel",
        ),
        (
            Message::Text(json!({"model": "mock-echo"}).to_string().into()),
            400,
            "unsupported websocket request type: (missing)",
        ),
        (
            Message::Text(
                json!({"type": "response.create", "input": []})
                    .to_string()
                    .into(),
            ),
            400,
            "missing model in response.create request",
        ),
        (
            Message::Text(
                json!({"type": "response.create", "model": "mock-echo", "input": "hi"})
                    .to_string()
                    .into(),
            ),
            400,
            "websocket request requires array field: input",
        ),
        (
            Message::Text(
                json!({"type": "response.append", "input": []})
                    .to_string()
                    .into(),
            ),
            400,
            "websocket request received before response.create",
        ),
        // Binary frames are read like text frames.
        (
            Message::Binary(json!({"type": "nope"}).to_string().into_bytes().into()),
            400,
            "unsupported websocket request type: nope",
        ),
    ];
    for (message, status, text) in cases {
        client.socket.send(message).await.unwrap();
        let error = client.next_json().await;
        assert_eq!(error["type"], "error", "{error}");
        assert_eq!(error["status"], status, "{error}");
        assert_eq!(error["error"]["message"], text, "{error}");
        assert_eq!(error["error"]["type"], "invalid_request_error", "{error}");
    }

    // The pipeline's own request errors do not end the connection either.
    client
        .send_json(&create("no-such-model", json!([user("hi")])))
        .await;
    let error = client.next_json().await;
    assert_eq!(error["status"], 404, "{error}");
    assert_eq!(error["error"]["code"], "model_not_found");

    // After all that, the socket still works.
    client
        .send_json(&create("mock-echo", json!([user("still here")])))
        .await;
    let frames = client.read_until(DONE).await;
    assert!(output_text(frames.last().unwrap()).contains("still here"));
}

#[tokio::test]
async fn previous_response_id_on_a_fresh_socket_is_a_409() {
    let server = TestServer::start().await;
    let mut client = connect(&server).await;
    client
        .send_json(&json!({"type": "response.create", "model": "recorded",
                           "previous_response_id": "resp_from_another_life",
                           "input": [user("continue")]}))
        .await;
    let error = client.next_json().await;
    assert_eq!(
        error,
        json!({"type": "error", "status": 409, "error": {
            "message": "Previous response is not available on this websocket; resend the full conversation input without previous_response_id",
            "type": "invalid_request_error",
            "code": "previous_response_not_found",
            "param": "previous_response_id"
        }})
    );
    assert!(server.fake.on("/v1/responses").is_empty());

    // The client does as told and the connection serves it.
    client
        .send_json(&create("recorded", json!([user("everything again")])))
        .await;
    let frames = client.read_until(DONE).await;
    assert_eq!(frames.last().unwrap()["type"], "response.completed");
}

#[tokio::test]
async fn tool_calls_and_outputs_are_paired_before_going_upstream() {
    let server = TestServer::start().await;

    // A call that gets its output: both go upstream.
    let mut client = connect(&server).await;
    client
        .send_json(&create("recorded", json!([user("please use tool")])))
        .await;
    let frames = client.read_until(DONE).await;
    let completed = frames.last().unwrap();
    let call = &completed["response"]["output"][0];
    assert_eq!(call["type"], "function_call", "{completed}");
    let call_id = call["call_id"].as_str().unwrap().to_string();
    client
        .send_json(&json!({
            "type": "response.create",
            "previous_response_id": completed["response"]["id"],
            "input": [{"type": "function_call_output", "call_id": call_id, "output": "42"}]
        }))
        .await;
    client.read_until(DONE).await;
    let seen = server.fake.on("/v1/responses");
    assert_eq!(
        summary(&seen[1].body["input"]),
        vec![
            "user:please use tool".to_string(),
            format!("function_call:{call_id}"),
            format!("function_call_output:{call_id}")
        ]
    );

    // A call that is never answered, and an output for a call nobody made:
    // neither reaches the upstream, which would reject the transcript.
    let mut client = connect(&server).await;
    client
        .send_json(&create("recorded", json!([user("please use tool")])))
        .await;
    let frames = client.read_until(DONE).await;
    let completed = frames.last().unwrap();
    assert_eq!(completed["response"]["output"][0]["type"], "function_call");
    client
        .send_json(&json!({
            "type": "response.create",
            "previous_response_id": completed["response"]["id"],
            "input": [
                {"type": "function_call_output", "call_id": "call_nobody_made", "output": "?"},
                user("never mind")
            ]
        }))
        .await;
    client.read_until(DONE).await;
    let seen = server.fake.on("/v1/responses");
    assert_eq!(
        summary(&seen[3].body["input"]),
        vec!["user:please use tool", "user:never mind"]
    );
}

#[tokio::test]
async fn requests_sent_during_a_turn_wait_their_turn() {
    let server = TestServer::start().await;
    let mut client = connect(&server).await;
    // Three requests at once; an unknown type in between is refused at once
    // without disturbing them.
    client
        .send_json(&create("recorded", json!([user("first")])))
        .await;
    client
        .send_json(
            &json!({"type": "response.create", "previous_response_id": "resp_1",
                           "input": [user("second")]}),
        )
        .await;
    client.send_json(&json!({"type": "session.update"})).await;
    client
        .send_json(&json!({"type": "response.append", "input": [user("third")]}))
        .await;

    let mut answers = Vec::new();
    let mut refused = 0;
    while answers.len() < 3 {
        let frame = client.next_json().await;
        match frame["type"].as_str() {
            Some("response.completed") => answers.push(output_text(&frame)),
            Some("error") => {
                assert_eq!(frame["status"], 400, "{frame}");
                refused += 1;
            }
            _ => {}
        }
    }
    assert_eq!(answers, vec!["answer 1", "answer 2", "answer 3"]);
    assert_eq!(refused, 1);
    let seen = server.fake.on("/v1/responses");
    assert_eq!(
        summary(&seen[2].body["input"]),
        vec![
            "user:first",
            "assistant:answer 1",
            "user:second",
            "assistant:answer 2",
            "user:third"
        ]
    );
}

#[tokio::test]
async fn an_upstream_failure_sends_an_error_frame_and_closes_1011() {
    let server = TestServer::start().await;
    let mut client = connect(&server).await;
    client
        .send_json(&create("mock-error-500", json!([user("hi")])))
        .await;
    let error = client.next_json().await;
    assert_eq!(error["type"], "error");
    let status = error["status"].as_u64().unwrap();
    assert!((500..600).contains(&status), "{error}");
    assert!(error["error"]["message"].is_string(), "{error}");
    match client.read_close().await {
        Received::Close(Some((code, _))) => assert_eq!(code, 1011),
        other => panic!("expected close 1011, got {other:?}"),
    }

    // A rate limit carries its wait inside the error object.
    let mut client = connect(&server).await;
    client
        .send_json(&create("mock-error-429", json!([user("hi")])))
        .await;
    let error = client.next_json().await;
    assert_eq!(error["status"], 429, "{error}");
    assert!(
        error["error"]["headers"]["retry-after"].is_string(),
        "{error}"
    );
    assert!(matches!(
        client.read_close().await,
        Received::Close(Some((1011, _)))
    ));
}

#[tokio::test]
async fn pings_are_answered_and_idle_connections_are_pinged() {
    let server = TestServer::with(Settings {
        keepalive_secs: 1,
        ..Settings::default()
    })
    .await;
    let mut client = connect(&server).await;

    client
        .socket
        .send(Message::Ping(b"are you there".to_vec().into()))
        .await
        .unwrap();
    assert_eq!(client.receive().await, Received::Pong);

    // Nothing happens for a second: the server checks on the client.
    let started = std::time::Instant::now();
    assert_eq!(client.receive().await, Received::Ping);
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(500) && waited < Duration::from_secs(3),
        "{waited:?}"
    );

    // The connection is as usable as before.
    client
        .send_json(&create("mock-echo", json!([user("after the ping")])))
        .await;
    let frames = client.read_until(DONE).await;
    assert!(output_text(frames.last().unwrap()).contains("after the ping"));
}

#[tokio::test]
async fn an_oversized_message_closes_1009() {
    // The limit is 1 MiB.
    let server = TestServer::start().await;
    let mut client = connect(&server).await;
    let huge = create("mock-echo", json!([user(&"x".repeat(1024 * 1024 + 4096))]));
    // The server may stop reading before all of it is sent.
    let _ = client
        .socket
        .send(Message::Text(huge.to_string().into()))
        .await;
    match client.read_close().await {
        Received::Close(Some((code, reason))) => {
            assert_eq!(code, 1009);
            assert_eq!(reason, "message too big");
        }
        other => panic!("expected close 1009, got {other:?}"),
    }

    // One byte under the limit is a perfectly good message.
    let mut client = connect(&server).await;
    let padding = "y".repeat(1024 * 1024 - 400);
    client
        .send_json(&create("mock-echo", json!([user(&padding)])))
        .await;
    let first = client.next_json().await;
    assert_eq!(first["type"], "response.created", "{first}");
}

#[tokio::test]
async fn server_shutdown_closes_idle_sockets_with_1001() {
    let mut server = TestServer::start().await;
    let mut client = connect(&server).await;
    client
        .send_json(&create("mock-echo", json!([user("hi")])))
        .await;
    client.read_until(DONE).await;

    server.begin_shutdown();
    match client.read_close().await {
        Received::Close(Some((code, _))) => assert_eq!(code, 1001),
        other => panic!("expected close 1001, got {other:?}"),
    }
    // The server does not wait out its grace period for a closed socket.
    let started = std::time::Instant::now();
    server.stopped().await;
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn server_shutdown_lets_the_turn_in_progress_finish() {
    let mut server = TestServer::start().await;
    let mut client = connect(&server).await;
    client
        .send_json(&create(
            "mock-slow",
            json!([user("one two three four five six")]),
        ))
        .await;
    assert_eq!(client.next_json().await["type"], "response.created");

    server.begin_shutdown();
    let frames = client.read_until(DONE).await;
    let completed = frames.last().unwrap();
    assert_eq!(completed["type"], "response.completed");
    assert!(output_text(completed).contains("one two three four five six"));
    assert!(matches!(
        client.read_close().await,
        Received::Close(Some((1001, _)))
    ));
    server.stopped().await;
}

#[tokio::test]
async fn a_client_that_leaves_mid_turn_cancels_the_turn() {
    let server = TestServer::start().await;
    let gauges = server.gateway.telemetry().gauges().clone();
    let mut client = connect(&server).await;
    let long = "word ".repeat(200);
    client
        .send_json(&create("mock-slow", json!([user(&long)])))
        .await;
    assert_eq!(client.next_json().await["type"], "response.created");
    assert_eq!(gauges.ws_connections(), 1);
    drop(client);

    eventually("the turn and the connection to be released", || {
        gauges.ws_connections() == 0 && gauges.active_streams() == 0 && gauges.in_flight() == 0
    })
    .await;
    let records = server.records();
    let turn = records
        .iter()
        .find(|record| record.endpoint == "WS /v1/responses")
        .expect("the turn is recorded");
    assert_eq!(turn.status, 499);
}

#[tokio::test]
async fn frames_of_a_named_lane_carry_its_stream_id() {
    let server = TestServer::start().await;
    let mut client = connect(&server).await;
    let mut request = create("recorded", json!([user("hi")]));
    request["stream_id"] = json!("lane-a");
    client.send_json(&request).await;
    let frames = client.read_until(DONE).await;
    assert_eq!(frames.last().unwrap()["type"], "response.completed");
    for frame in &frames {
        assert_eq!(frame["stream_id"], "lane-a", "{frame}");
    }
    assert!(
        server.fake.on("/v1/responses")[0]
            .body
            .get("stream_id")
            .is_none()
    );

    // Errors about a lane's request are tagged too.
    client
        .send_json(&json!({"type": "response.create", "stream_id": "lane-b", "input": "x"}))
        .await;
    let error = client.next_json().await;
    assert_eq!(error["status"], 400);
    assert_eq!(error["stream_id"], "lane-b");
}

#[tokio::test]
async fn the_upgrade_is_authenticated_like_any_request() {
    let server = TestServer::start().await;

    let refused = ws_connect(&server.ws_url("/v1/responses"), &[]).await;
    match refused {
        Err(WsError::Http(response)) => {
            assert_eq!(response.status(), 401);
            let body: Value = serde_json::from_slice(response.body().as_deref().unwrap()).unwrap();
            assert_eq!(body["error"]["type"], "authentication_error");
        }
        other => panic!("expected an HTTP 401, got {:?}", other.map(|_| "a socket")),
    }
    let refused = ws_connect(
        &server.ws_url("/v1/responses"),
        &[("authorization", "Bearer sy-wrong")],
    )
    .await;
    assert!(matches!(refused, Err(WsError::Http(response)) if response.status() == 401));

    // Every key location works for the upgrade.
    let by_header = ws_connect(&server.ws_url("/v1/responses"), &[("x-api-key", KEY)]).await;
    assert!(by_header.is_ok());
    let by_query = ws_connect(&server.ws_url(&format!("/v1/responses?key={KEY}")), &[]).await;
    assert!(by_query.is_ok());

    // A plain GET is told to upgrade.
    let response = http()
        .get(server.url("/v1/responses"))
        .bearer_auth(KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 426);
    assert_eq!(
        response.headers().get("upgrade").unwrap().to_str().unwrap(),
        "websocket"
    );
    assert!(response.headers().contains_key("x-request-id"));
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "upgrade_required");
}
