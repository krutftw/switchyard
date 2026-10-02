//! Review findings for `reasoning_store` (R-S1). The first test asserts the
//! correct behaviour and failed against the implementation as it was
//! reviewed; the finding is fixed and the test is kept as a regression test
//! (its doc comment describes the old defect). The second one is a guard
//! rail for whoever changes the placement logic. `restore_order.rs` checks
//! the same property on random turns.

use std::time::Duration;
use switchyard_core::ir::{
    FinishReason, Message, Part, Reasoning, Request, Response, Role, Signature,
};
use switchyard_core::protocol::Protocol;
use switchyard_translate::reasoning_store::ReasoningStore;

fn signed(text: &str) -> Part {
    Part::Reasoning(Reasoning {
        id: None,
        text: text.into(),
        signature: Some(Signature::new(
            Protocol::OpenaiResponses,
            format!("enc-{text}"),
        )),
        redacted: false,
    })
}

fn call(id: &str) -> Part {
    Part::tool_call(id, "lookup", "{}")
}

fn kinds(parts: &[Part]) -> Vec<String> {
    parts
        .iter()
        .map(|p| match p {
            Part::Reasoning(r) => format!("R:{}", r.text),
            Part::ToolCall(c) => format!("C:{}", c.id),
            Part::Text(t) => format!("T:{}", t.text),
            _ => "other".to_string(),
        })
        .collect()
}

fn remembered(parts: Vec<Part>) -> ReasoningStore {
    let store = ReasoningStore::new(64, Duration::from_secs(3600));
    let mut response = Response::new("resp_1", "upstream-model");
    response.parts = parts;
    response.finish = FinishReason::ToolCalls;
    store.remember(&response, "key");
    store
}

fn assistant_turn(parts: Vec<Part>) -> Request {
    let mut request = Request::new("m", Protocol::OpenaiChat);
    request.messages = vec![
        Message::user_text("question"),
        Message::new(Role::Assistant, parts),
    ];
    request
}

/// R-S1. Restored reasoning parts must keep their original order.
///
/// `ReasoningStore::restore` documents that the remembered parts "are
/// inserted again, in their original order", and the track requirement is to
/// re-insert "the stored Reasoning parts" in front of the tool calls. The
/// placement walks back over `gap` non-reasoning parts counted in the
/// *response*; when the client's turn has fewer of them between the
/// reasoning and its call than the response had, the walk runs past the
/// previous tool call and past reasoning that was restored for it.
///
/// The turn below is what a Chat Completions client sends back for an
/// OpenAI Responses output of
/// `reasoning, function_call, reasoning, message, message, function_call`:
/// Chat has one `content` string (so the two messages arrive as one text
/// part, in front of the calls) and no slot for reasoning items. The second
/// reasoning item ends up in front of the first one and in front of the text
/// — at the very start of the turn, ahead of a call it was generated after.
#[test]
fn restored_reasoning_keeps_its_original_order() {
    let store = remembered(vec![
        signed("first"),
        call("call_1"),
        signed("second"),
        Part::text("Checking "),
        Part::text("one more thing."),
        call("call_2"),
    ]);

    let mut request = assistant_turn(vec![
        Part::text("Checking one more thing."),
        call("call_1"),
        call("call_2"),
    ]);
    let restored = store.restore(&mut request, Protocol::OpenaiResponses, "key");
    assert_eq!(restored.reasoning_parts, 2);

    let after = kinds(&request.messages[1].parts);
    let first = after.iter().position(|k| k == "R:first");
    let second = after.iter().position(|k| k == "R:second");
    assert!(
        first < second,
        "reasoning parts were restored out of order: {after:?}"
    );
}

/// Passing guard: when the client only stripped the reasoning (every other
/// part is still where the response had it) the turn is rebuilt exactly, and
/// a second pass is a no-op. A fix for R-S1 must keep this true.
#[test]
fn restore_round_trips_a_turn_whose_reasoning_was_stripped() {
    let original = vec![
        signed("a"),
        Part::text("intro"),
        call("c1"),
        signed("b"),
        call("c2"),
        call("c3"),
    ];
    let store = remembered(original.clone());
    let stripped: Vec<Part> = original
        .iter()
        .filter(|p| !matches!(p, Part::Reasoning(_)))
        .cloned()
        .collect();
    let mut request = assistant_turn(stripped);
    store.restore(&mut request, Protocol::OpenaiResponses, "key");
    assert_eq!(request.messages[1].parts, original);

    // A second pass changes nothing.
    assert!(
        store
            .restore(&mut request, Protocol::OpenaiResponses, "key")
            .is_empty()
    );
    assert_eq!(request.messages[1].parts, original);
}
