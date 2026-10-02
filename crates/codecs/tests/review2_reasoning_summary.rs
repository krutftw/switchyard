//! Review finding (round 2): a Chat client's `reasoning_effort` and the
//! summary a Responses upstream has to be asked for.
//!
//! Notes 12 §8.1, format `openai` (implicit): "if nothing explicit: string
//! `reasoning_effort`: … `none` -> Disabled; anything else ->
//! Enabled(`auto`)". §8.1 "Translated" reader: "explicit-only when source is
//! Chat and target is Claude …, the full reader otherwise". §8.3,
//! `openai-response`: "Enabled -> `reasoning.summary = detail` normalised to
//! `concise | detailed | auto` (default `auto`)".
//!
//! A Chat Completions client has no field that asks for the reasoning text:
//! servers of that protocol send `reasoning_content` whenever the model
//! reasons. An OpenAI Responses upstream only returns summaries when
//! `reasoning.summary` is set; without it every reasoning item arrives with
//! `summary: []`, so the client gets no `reasoning_content` at all although
//! it turned reasoning on.
//!
//! The first review round reported exactly this for a Gemini upstream
//! (`includeThoughts`), and it was fixed there, inside the Gemini encoder
//! only. The Responses encoder still writes the effort alone. Matrix (f)
//! does not see it: `matrix_reasoning.rs` compares `reasoning.effort` and
//! ignores `reasoning.summary`.

mod support;

use serde_json::{Value, json};
use support::harness::{CHAT, GEMINI, RESPONSES, known_caps, translate_request};
use switchyard_core::UpstreamCtx;

fn chat_request(effort: &str) -> Value {
    json!({
        "model": "gpt-5.5",
        "messages": [{"role": "user", "content": "Prove that 91 is not prime."}],
        "reasoning_effort": effort
    })
}

#[test]
fn a_chat_reasoning_effort_asks_a_responses_upstream_for_summaries() {
    let caps = known_caps(RESPONSES);
    let mut failures = Vec::new();
    for (label, ctx) in [
        ("known model", caps.ctx()),
        ("unknown model", UpstreamCtx::default()),
    ] {
        for effort in ["low", "medium", "high"] {
            let body = translate_request(CHAT, RESPONSES, &chat_request(effort), &ctx)
                .expect("the request translates");
            assert_eq!(
                body["reasoning"]["effort"], effort,
                "{label}: the depth itself is translated: {}",
                body["reasoning"]
            );
            if body["reasoning"]["summary"] != "auto" {
                failures.push(format!(
                    "{label}, reasoning_effort {effort}: `reasoning` is {} (notes 12 §8.3 expects `summary: \"auto\"`)",
                    body["reasoning"]
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The two neighbours of the rule, which hold today: `none` asks for no
/// summary, and the same Chat request does ask a Gemini upstream for its
/// thoughts (the round-1 fix).
#[test]
fn the_neighbouring_cases_hold() {
    let caps = known_caps(RESPONSES);
    let body = translate_request(CHAT, RESPONSES, &chat_request("none"), &caps.ctx())
        .expect("the request translates");
    assert!(
        body["reasoning"].get("summary").is_none(),
        "`none` must not ask for a summary: {}",
        body["reasoning"]
    );
    let caps = known_caps(GEMINI);
    let body = translate_request(CHAT, GEMINI, &chat_request("high"), &caps.ctx())
        .expect("the request translates");
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["includeThoughts"],
        true
    );
}
