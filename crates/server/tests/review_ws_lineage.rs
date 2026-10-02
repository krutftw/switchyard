//! Review: conversation lineage on the Responses WebSocket.
//!
//! `previous_response_id` is what says "continue that response". The vendor
//! protocol (notes `15-api-research.md` §1.4 / §1.5, both verified against
//! the official guide) is explicit:
//!
//! * "`stream_id` controls routing only; `previous_response_id` controls
//!   conversation lineage. Reusing a `stream_id` without
//!   `previous_response_id` starts a NEW conversation."
//! * recovery after a cache miss: "send `previous_response_id: null` (or
//!   omit) with the FULL input context".
//!
//! So a `response.create` that names no previous response is a request of
//! its own. The gateway instead prepends the whole stored conversation to
//! every such request unless its input happens to contain an assistant
//! message or a tool call (`ws/transcript.rs`, the `(Some(last), None)`
//! arm). Independent requests on one socket leak into each other, and a
//! client that resends its full context gets it duplicated whenever that
//! context holds user/developer messages only (a conversation rewound to
//! its first message, for instance).
//!
//! `docs/DESIGN.md` §10 lists exactly two follow-up cases that merge —
//! `previous_response_id` and `response.append` — and the notes say that
//! where vendor documentation and the reference disagree, the vendor wins
//! for new code.

mod support;

use serde_json::{Value, json};
use support::{KEY, TestServer, WsClient, ws_connect};

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

/// `role:text` of every message of an upstream `input`.
fn summary(input: &Value) -> Vec<String> {
    input
        .as_array()
        .expect("input is an array")
        .iter()
        .map(|item| {
            let text: String = match &item["content"] {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect(),
                _ => String::new(),
            };
            format!("{}:{text}", item["role"].as_str().unwrap_or("?"))
        })
        .collect()
}

/// Two unrelated requests on one connection, the way an SDK user sends
/// them (`connection.response.create(model=…, input=[…])` twice): neither
/// names a previous response, so neither continues anything.
#[tokio::test]
async fn a_create_without_previous_response_id_is_a_request_of_its_own() {
    let server = TestServer::start().await;
    let mut client = connect(&server).await;

    client
        .send_json(&json!({"type": "response.create", "model": "recorded",
                           "input": [user("translate cat into French")]}))
        .await;
    let frames = client.read_until(DONE).await;
    assert_eq!(frames.last().unwrap()["type"], "response.completed");

    client
        .send_json(&json!({"type": "response.create", "model": "recorded",
                           "input": [user("what is the capital of Peru?")]}))
        .await;
    let frames = client.read_until(DONE).await;
    assert_eq!(frames.last().unwrap()["type"], "response.completed");

    let seen = server.fake.on("/v1/responses");
    assert_eq!(seen.len(), 2);
    assert_eq!(
        summary(&seen[1].body["input"]),
        vec!["user:what is the capital of Peru?"],
        "the second request names no previous response, yet the first request and its answer \
         were sent upstream in front of it"
    );
}

/// A client that resends its full context (no `previous_response_id`) after
/// rewinding the conversation to its first message: the context is the
/// whole conversation, not an increment to the abandoned one.
#[tokio::test]
async fn a_resent_full_context_of_user_messages_only_is_not_appended_to_the_old_conversation() {
    let server = TestServer::start().await;
    let mut client = connect(&server).await;

    client
        .send_json(&json!({"type": "response.create", "model": "recorded",
                           "instructions": "be brief",
                           "input": [user("plan a trip to Rome")]}))
        .await;
    let first = client.read_until(DONE).await;
    client
        .send_json(&json!({"type": "response.create",
                           "previous_response_id": first.last().unwrap()["response"]["id"],
                           "input": [user("make it five days")]}))
        .await;
    client.read_until(DONE).await;

    // The user edits the first message and resubmits: full context, no id.
    client
        .send_json(&json!({"type": "response.create", "model": "recorded",
                           "instructions": "be brief",
                           "input": [user("plan a trip to Oslo")]}))
        .await;
    let frames = client.read_until(DONE).await;
    assert_eq!(frames.last().unwrap()["type"], "response.completed");

    let seen = server.fake.on("/v1/responses");
    assert_eq!(seen.len(), 3);
    assert_eq!(
        summary(&seen[2].body["input"]),
        vec!["user:plan a trip to Oslo"],
        "the abandoned conversation was sent upstream in front of the new one"
    );
}

/// `previous_response_id` names a response; the gateway only has the
/// latest one. Naming any other (an earlier response of this connection —
/// "regenerate", a fork) must not be answered as if the latest had been
/// named: either the named response is continued, or the client is told it
/// is not available (`previous_response_not_found`, as on a fresh socket)
/// so that it resends its context.
#[tokio::test]
async fn a_previous_response_id_other_than_the_latest_is_not_silently_continued_from_the_latest() {
    let server = TestServer::start().await;
    let mut client = connect(&server).await;

    client
        .send_json(&json!({"type": "response.create", "model": "recorded",
                           "input": [user("one")]}))
        .await;
    let first = client.read_until(DONE).await;
    let first_id = first.last().unwrap()["response"]["id"].clone();
    assert!(first_id.is_string());

    client
        .send_json(
            &json!({"type": "response.create", "previous_response_id": first_id,
                           "input": [user("two")]}),
        )
        .await;
    let second = client.read_until(DONE).await;
    assert_eq!(second.last().unwrap()["type"], "response.completed");
    assert_ne!(second.last().unwrap()["response"]["id"], first_id);

    // Regenerate the second answer: continue from the FIRST response again.
    client
        .send_json(
            &json!({"type": "response.create", "previous_response_id": first_id,
                           "input": [user("two, rephrased")]}),
        )
        .await;
    let third = client.read_until(DONE).await;
    let last = third.last().unwrap();

    if last["type"] == "error" {
        assert_eq!(
            last["error"]["code"], "previous_response_not_found",
            "{last}"
        );
        assert_eq!(server.fake.on("/v1/responses").len(), 2);
        return;
    }
    let seen = server.fake.on("/v1/responses");
    assert_eq!(seen.len(), 3);
    assert_eq!(
        summary(&seen[2].body["input"]),
        vec!["user:one", "assistant:answer 1", "user:two, rephrased"],
        "the request named the first response, but was continued from the second"
    );
}
