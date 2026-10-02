//! The pure helpers behind the Responses WebSocket transcript.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_responses::{
    is_transcript_replacement, merge_transcript, prewarm_frames, repair_tool_pairs, ws_error_frame,
};

fn user(id: &str, text: &str) -> Value {
    json!({"type": "message", "role": "user", "id": id, "content": text})
}

fn call(id: &str, call_id: &str) -> Value {
    json!({"type": "function_call", "id": id, "call_id": call_id, "name": "get", "arguments": "{}"})
}

fn output(call_id: &str, text: &str) -> Value {
    json!({"type": "function_call_output", "call_id": call_id, "output": text})
}

// ---------------------------------------------------------------------------
// merge_transcript
// ---------------------------------------------------------------------------

#[test]
fn merge_transcript_concatenates_input_output_and_new_input() {
    // The worked example: turn 1 asked for a tool, turn 2 delivers its output.
    let prev_input = vec![user("u1", "weather?")];
    let prev_output = vec![call("fc1", "c1")];
    let new_input = vec![output("c1", "sunny")];
    assert_eq!(
        merge_transcript(&prev_input, &prev_output, &new_input),
        vec![
            user("u1", "weather?"),
            call("fc1", "c1"),
            output("c1", "sunny")
        ]
    );
    assert_eq!(merge_transcript(&[], &[], &[]), Vec::<Value>::new());
}

#[test]
fn merge_transcript_drops_later_tool_calls_with_a_known_call_id() {
    // The client replays the call it is answering; the first copy wins even
    // though the replay looks different.
    let replayed =
        json!({"type": "function_call", "call_id": "c1", "name": "get", "arguments": "{ }"});
    let custom = json!({"type": "custom_tool_call", "id": "ctc1", "call_id": "c2", "name": "exec", "input": "ls"});
    let merged = merge_transcript(
        &[user("u1", "go")],
        &[call("fc1", "c1"), custom.clone()],
        &[
            replayed,
            custom.clone(),
            output("c1", "ok"),
            output("c1", "ok again"),
        ],
    );
    assert_eq!(
        merged,
        vec![
            user("u1", "go"),
            call("fc1", "c1"),
            custom,
            // Outputs are never de-duplicated by call id.
            output("c1", "ok"),
            output("c1", "ok again"),
        ]
    );

    // Calls without a call id cannot be matched and are all kept.
    let anonymous = json!({"type": "function_call", "name": "get", "arguments": "{}"});
    assert_eq!(
        merge_transcript(
            std::slice::from_ref(&anonymous),
            &[],
            std::slice::from_ref(&anonymous)
        ),
        vec![anonymous.clone(), anonymous.clone()]
    );
}

#[test]
fn merge_transcript_keeps_the_last_occurrence_of_an_item_id() {
    let edited = user("u1", "weather tomorrow?");
    let merged = merge_transcript(
        &[user("u1", "weather?"), user("u2", "second")],
        &[json!({"type": "message", "role": "assistant", "id": "msg_1", "content": "Sunny."})],
        &[
            edited.clone(),
            json!({"type": "message", "role": "user", "content": "no id, kept"}),
            json!({"type": "message", "role": "user", "content": "no id, kept"}),
        ],
    );
    assert_eq!(
        merged,
        vec![
            user("u2", "second"),
            json!({"type": "message", "role": "assistant", "id": "msg_1", "content": "Sunny."}),
            edited,
            json!({"type": "message", "role": "user", "content": "no id, kept"}),
            json!({"type": "message", "role": "user", "content": "no id, kept"}),
        ]
    );
}

#[test]
fn merge_transcript_never_replaces_a_referenced_call_with_an_unreferenced_one() {
    // Two different calls share an item id. The first is answered by an
    // output; the later one is not. Keeping "the last" would orphan the
    // output, so the referenced one stays.
    let answered = call("fc_same", "c1");
    let dangling = call("fc_same", "c2");
    let merged = merge_transcript(
        &[],
        std::slice::from_ref(&answered),
        &[output("c1", "ok"), dangling.clone()],
    );
    assert_eq!(merged, vec![answered.clone(), output("c1", "ok")]);

    // When the later one is the referenced call, last-wins applies as usual.
    let merged = merge_transcript(
        &[],
        &[dangling],
        &[call("fc_same", "c1"), output("c1", "ok")],
    );
    assert_eq!(merged, vec![call("fc_same", "c1"), output("c1", "ok")]);

    // Both referenced: the last occurrence is kept.
    let merged = merge_transcript(
        &[],
        &[call("fc_same", "c1"), output("c1", "one")],
        &[call("fc_same", "c2"), output("c2", "two")],
    );
    assert_eq!(
        merged,
        vec![
            output("c1", "one"),
            call("fc_same", "c2"),
            output("c2", "two")
        ]
    );
}

// ---------------------------------------------------------------------------
// is_transcript_replacement
// ---------------------------------------------------------------------------

#[test]
fn transcript_replacement_is_detected_by_replayed_model_output() {
    assert!(is_transcript_replacement(&[
        user("u1", "hi"),
        call("fc1", "c1"),
        output("c1", "x")
    ]));
    assert!(is_transcript_replacement(&[
        json!({"type": "custom_tool_call", "call_id": "c", "name": "e", "input": ""})
    ]));
    assert!(is_transcript_replacement(&[
        user("u1", "hi"),
        json!({"type": "message", "role": "assistant", "content": "hello"}),
    ]));

    // Increments: new user input and tool outputs only.
    assert!(!is_transcript_replacement(&[]));
    assert!(!is_transcript_replacement(&[user("u2", "next question")]));
    assert!(!is_transcript_replacement(&[output("c1", "sunny")]));
    assert!(!is_transcript_replacement(&[
        json!({"type": "reasoning", "id": "rs_1", "summary": []})
    ]));
    assert!(!is_transcript_replacement(&[
        json!({"role": "user", "content": "short form"})
    ]));
    assert!(!is_transcript_replacement(&[
        json!("stray string"),
        json!(null)
    ]));
    // The short message form (no `type`) is an assistant message all the same.
    assert!(is_transcript_replacement(&[
        json!({"role": "assistant", "content": "hello"})
    ]));
}

// ---------------------------------------------------------------------------
// repair_tool_pairs
// ---------------------------------------------------------------------------

#[test]
fn repair_drops_orphaned_calls_and_orphaned_outputs() {
    let custom_call =
        json!({"type": "custom_tool_call", "call_id": "c3", "name": "exec", "input": "ls"});
    let custom_output = json!({"type": "custom_tool_call_output", "call_id": "c3", "output": "ok"});
    let reasoning =
        json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "gAAAA"});
    let repaired = repair_tool_pairs(vec![
        user("u1", "go"),
        reasoning.clone(),
        call("fc1", "c1"),
        output("c1", "paired"),
        // No output for this call anywhere in the input.
        call("fc2", "c2"),
        // No call for this output.
        output("c_lost", "orphan"),
        custom_call.clone(),
        custom_output.clone(),
        json!({"type": "custom_tool_call", "call_id": "c4", "name": "exec", "input": "pwd"}),
        json!({"type": "custom_tool_call_output", "call_id": "c5", "output": "orphan"}),
        user("u2", "and then?"),
    ]);
    assert_eq!(
        repaired,
        vec![
            user("u1", "go"),
            reasoning,
            call("fc1", "c1"),
            output("c1", "paired"),
            custom_call,
            custom_output,
            user("u2", "and then?"),
        ]
    );
}

#[test]
fn repair_handles_items_without_call_ids() {
    let named_result =
        json!({"type": "function_call_output", "name": "heartbeat", "output": "alive"});
    let repaired = repair_tool_pairs(vec![
        // A call without a call id is rejected upstream.
        json!({"type": "function_call", "id": "fc_x", "name": "get", "arguments": "{}"}),
        json!({"type": "function_call", "call_id": "  ", "name": "get", "arguments": "{}"}),
        // A standalone *named* result is a deliberate client shape.
        named_result.clone(),
        json!({"type": "function_call_output", "output": "anonymous"}),
        json!({"type": "custom_tool_call_output", "name": "exec", "output": "anonymous"}),
    ]);
    assert_eq!(repaired, vec![named_result]);
}

#[test]
fn repair_leaves_a_healthy_transcript_alone_and_collapses_duplicate_ids() {
    let healthy = vec![
        user("u1", "go"),
        call("fc1", "c1"),
        call("fc2", "c2"),
        output("c2", "two"),
        output("c1", "one"),
        json!({"type": "web_search_call", "id": "ws_1", "status": "completed"}),
        json!({"type": "message", "role": "assistant", "content": "done"}),
    ];
    assert_eq!(repair_tool_pairs(healthy.clone()), healthy);
    assert_eq!(repair_tool_pairs(Vec::new()), Vec::<Value>::new());

    let deduped = repair_tool_pairs(vec![user("u1", "old"), user("u1", "new")]);
    assert_eq!(deduped, vec![user("u1", "new")]);
}

// ---------------------------------------------------------------------------
// ws_error_frame
// ---------------------------------------------------------------------------

#[test]
fn error_frame_shapes() {
    assert_eq!(
        ws_error_frame(
            400,
            "websocket request requires array field: input",
            None,
            None
        ),
        json!({
            "type": "error",
            "status": 400,
            "error": {"message": "websocket request requires array field: input", "type": "invalid_request_error"}
        })
    );
    assert_eq!(
        ws_error_frame(
            409,
            "Previous response is not available on this websocket; resend the full conversation input without previous_response_id",
            Some("previous_response_not_found"),
            Some("previous_response_id"),
        ),
        json!({
            "type": "error",
            "status": 409,
            "error": {
                "message": "Previous response is not available on this websocket; resend the full conversation input without previous_response_id",
                "type": "invalid_request_error",
                "code": "previous_response_not_found",
                "param": "previous_response_id"
            }
        })
    );
}

#[test]
fn error_frame_type_and_code_follow_the_status() {
    let detail = |status: u16| {
        let frame = ws_error_frame(status, "m", None, None);
        assert_eq!(frame["type"], json!("error"));
        (
            frame["status"].as_u64().unwrap(),
            frame["error"]["type"].as_str().unwrap().to_string(),
            frame["error"]
                .get("code")
                .and_then(Value::as_str)
                .map(str::to_string),
        )
    };
    let expect = |status: u64, kind: &str, code: Option<&str>| {
        (status, kind.to_string(), code.map(str::to_string))
    };
    assert_eq!(
        detail(401),
        expect(401, "authentication_error", Some("invalid_api_key"))
    );
    assert_eq!(
        detail(403),
        expect(403, "permission_error", Some("insufficient_quota"))
    );
    assert_eq!(
        detail(404),
        expect(404, "invalid_request_error", Some("model_not_found"))
    );
    assert_eq!(
        detail(408),
        expect(408, "server_error", Some("request_timeout"))
    );
    assert_eq!(detail(413), expect(413, "invalid_request_error", None));
    assert_eq!(
        detail(429),
        expect(429, "rate_limit_error", Some("rate_limit_exceeded"))
    );
    assert_eq!(
        detail(500),
        expect(500, "server_error", Some("internal_server_error"))
    );
    assert_eq!(
        detail(502),
        expect(502, "server_error", Some("internal_server_error"))
    );
    // Not an HTTP error status: reported as an internal error.
    assert_eq!(
        detail(0),
        expect(500, "server_error", Some("internal_server_error"))
    );
    assert_eq!(
        detail(200),
        expect(500, "server_error", Some("internal_server_error"))
    );

    // The caller's code wins; blank values count as absent.
    let frame = ws_error_frame(429, "m", Some("insufficient_quota"), Some(" "));
    assert_eq!(frame["error"]["code"], json!("insufficient_quota"));
    assert_eq!(frame["error"].get("param"), None);
}

// ---------------------------------------------------------------------------
// prewarm_frames
// ---------------------------------------------------------------------------

#[test]
fn prewarm_frames_answer_a_generate_false_request_locally() {
    let (created, completed) = prewarm_frames("gpt-5", 1_767_225_600);
    let id = created["response"]["id"].as_str().unwrap().to_string();
    assert!(
        id.starts_with("resp_prewarm_") && id.len() > "resp_prewarm_".len(),
        "{id}"
    );
    assert_eq!(
        created,
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "id": id, "object": "response", "created_at": 1767225600, "status": "in_progress",
                "background": false, "error": null, "output": [], "model": "gpt-5"
            }
        })
    );
    assert_eq!(
        completed,
        json!({
            "type": "response.completed",
            "sequence_number": 1,
            "response": {
                "id": id, "object": "response", "created_at": 1767225600, "status": "completed",
                "background": false, "error": null, "output": [],
                "usage": {
                    "input_tokens": 0, "input_tokens_details": {"cached_tokens": 0},
                    "output_tokens": 0, "output_tokens_details": {"reasoning_tokens": 0},
                    "total_tokens": 0
                },
                "model": "gpt-5"
            }
        })
    );

    // Every prewarm gets its own id; an unknown model is simply left out.
    let (other, _) = prewarm_frames("", 1);
    assert_ne!(other["response"]["id"], created["response"]["id"]);
    assert_eq!(other["response"].get("model"), None);
}
