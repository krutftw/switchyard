//! Matrix (d): a tool loop with reasoning, across two turns.
//!
//! Turn 1: upstream `U` answers with reasoning and a tool call, carrying its
//! opaque signature blob in the vendor's native position. The answer is
//! encoded for client `C`. Turn 2: the client sends the conversation back
//! exactly as a faithful `C` client would — echoing the assistant message it
//! received, adding the tool result.
//!
//! * Translated for `U` again, the request must carry `U`'s blob back in its
//!   native position: this is what lets the model continue its reasoning.
//! * Translated for any other upstream `V`, the request must not contain the
//!   blob anywhere: a blob is only ever replayed to the vendor that issued
//!   it (`docs/DESIGN.md` §2), and another vendor answers a foreign blob with
//!   a 400 that no failover can fix.
//!
//! One kind of blob has no field in two of the client protocols: a signature
//! that sits on the tool call itself (Gemini's `thoughtSignature` on a
//! `functionCall` part, the same thing on Google's Chat-compatible endpoint).
//! Chat and Gemini clients have a field for it. For Anthropic and Responses
//! clients the codecs carry it on a text-less `thinking` block / summary-less
//! `reasoning` item directly ahead of the call, marked as a call signature
//! (`views::client_call_blob`), and the request decoder puts it back on the
//! call. So every client returns every blob, and this test holds all sixteen
//! pairs to the same standard. For the two carrier clients it checks in
//! addition:
//!
//! * the answer shows the carrier directly ahead of the call, and the blob
//!   nowhere else;
//! * a client that does not echo the carrier (one that strips thinking it
//!   cannot display) still gets a request its upstream accepts: an unsigned
//!   call for a Chat upstream, the documented bypass value for Gemini (notes
//!   09 §7.2).
//!
//! The tool-call id must survive the trip through the client unchanged in
//! every case: the gateway's `reasoning_store` keys remembered signatures on
//! it (`docs/DESIGN.md` §4).
//!
//! Both bodies must pass their vendor's validator.

mod support;

use serde_json::{Value, json};
use support::harness::{
    ANTHROPIC, CHAT, Failures, GEMINI, RESPONSES, client_ctx, decode_request, known_caps, short,
    translate_request, validate_response, validate_translated_request,
};
use support::scenarios::{self, UpstreamResponse};
use support::views::{carrier_ahead_of_call, client_call_blob, has_call_slot};
use switchyard_codecs::codec;
use switchyard_core::Protocol;
use switchyard_core::ir::Message;

const TOOL_OUTPUT: &str = "15 degrees and cloudy";

/// What Gemini accepts in place of a thought signature it did not issue
/// (notes 09 §7.2).
const GEMINI_BYPASS: &str = "skip_thought_signature_validator";

/// Whether the fixture's blob sits on the tool call itself rather than on a
/// reasoning block.
fn blob_on_call(upstream: Protocol, fixture: &UpstreamResponse) -> bool {
    upstream == GEMINI || fixture.name == "signed_tool_call"
}

/// The answer an Anthropic or a Responses client received, without the block
/// that carries `carrier`: what a client that does not echo such blocks
/// sends back.
fn without_carrier(client: Protocol, answer: &Value, carrier: &str) -> Value {
    let mut answer = answer.clone();
    let (key, field) = match client {
        Protocol::Anthropic => ("content", "signature"),
        _ => ("output", "encrypted_content"),
    };
    if let Some(blocks) = answer[key].as_array_mut() {
        blocks.retain(|block| block[field] != carrier);
    }
    answer
}

/// What a request for `upstream` looks like when the signature of its tool
/// call did not come back through the client.
fn unsigned_call(upstream: Protocol, body: &Value) -> Result<(), String> {
    match upstream {
        Protocol::Gemini => {
            let parts = body["contents"][1]["parts"]
                .as_array()
                .ok_or("no model parts")?;
            let call = parts
                .iter()
                .find(|part| part.get("functionCall").is_some())
                .ok_or("no functionCall part")?;
            if call["thoughtSignature"] != GEMINI_BYPASS {
                return Err(format!(
                    "the replayed functionCall is signed with {}",
                    call["thoughtSignature"]
                ));
            }
            Ok(())
        }
        Protocol::OpenaiChat => {
            let call = &body["messages"][1]["tool_calls"][0];
            if !call.is_object() {
                return Err("the tool call is missing".into());
            }
            if call.get("extra_content").is_some() {
                return Err(format!("the replayed tool call is signed: {call}"));
            }
            Ok(())
        }
        other => Err(format!("{other} does not sign tool calls")),
    }
}

/// The turn-1 request of a `C` client: the tool request of the scenario
/// library, with reasoning switched on in `C`'s native spelling.
fn turn1(client: Protocol) -> Value {
    let mut body = scenarios::tool_request(client);
    let root = body.as_object_mut().expect("an object");
    match client {
        Protocol::OpenaiChat => {
            root.insert("reasoning_effort".into(), json!("high"));
        }
        Protocol::OpenaiResponses => {
            root.insert(
                "reasoning".into(),
                json!({"effort": "high", "summary": "auto"}),
            );
        }
        Protocol::Anthropic => {
            root.insert("max_tokens".into(), json!(16000));
            root.insert(
                "thinking".into(),
                json!({"type": "enabled", "budget_tokens": 4096}),
            );
        }
        Protocol::Gemini => {
            root.insert(
                "generationConfig".into(),
                json!({"thinkingConfig": {"thinkingBudget": 4096, "includeThoughts": true}}),
            );
        }
    }
    body
}

/// The turn-2 request: turn 1 plus the assistant message exactly as the
/// client received it (`answer` is the body `C.encode_response` produced)
/// plus the tool result.
fn turn2(client: Protocol, answer: &Value) -> Value {
    let mut body = turn1(client);
    match client {
        Protocol::OpenaiChat => {
            let message = answer["choices"][0]["message"].clone();
            let call_id = message["tool_calls"][0]["id"].clone();
            let messages = body["messages"].as_array_mut().expect("messages");
            messages.push(message);
            messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": TOOL_OUTPUT}));
        }
        Protocol::OpenaiResponses => {
            let output = answer["output"].as_array().expect("output").clone();
            let call_id = output
                .iter()
                .find(|item| item["type"] == "function_call")
                .map(|item| item["call_id"].clone())
                .expect("a function_call item");
            let input = body["input"].as_array_mut().expect("input");
            input.extend(output);
            input.push(
                json!({"type": "function_call_output", "call_id": call_id, "output": TOOL_OUTPUT}),
            );
        }
        Protocol::Anthropic => {
            let content = answer["content"].clone();
            let tool_use_id = content
                .as_array()
                .and_then(|blocks| blocks.iter().find(|block| block["type"] == "tool_use"))
                .map(|block| block["id"].clone())
                .expect("a tool_use block");
            let messages = body["messages"].as_array_mut().expect("messages");
            messages.push(json!({"role": "assistant", "content": content}));
            messages.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tool_use_id, "content": TOOL_OUTPUT}
            ]}));
        }
        Protocol::Gemini => {
            let content = answer["candidates"][0]["content"].clone();
            let call = content["parts"]
                .as_array()
                .and_then(|parts| parts.iter().find_map(|part| part.get("functionCall")))
                .cloned()
                .expect("a functionCall part");
            let mut response = json!({"name": call["name"], "response": {"output": TOOL_OUTPUT}});
            if let Some(id) = call.get("id") {
                response["id"] = id.clone();
            }
            let contents = body["contents"].as_array_mut().expect("contents");
            contents.push(content);
            contents.push(json!({"role": "user", "parts": [{"functionResponse": response}]}));
        }
    }
    body
}

/// Whether `body`, a request for upstream `U`, carries `blob` in the
/// position `U` issued it in.
fn in_native_position(
    upstream: Protocol,
    fixture: &UpstreamResponse,
    body: &Value,
    blob: &str,
) -> Result<(), String> {
    match upstream {
        // A signed thinking block opening the assistant turn, ahead of the
        // tool_use it led to.
        Protocol::Anthropic => {
            let blocks = body["messages"][1]["content"]
                .as_array()
                .ok_or("no assistant content")?;
            let thinking = blocks
                .iter()
                .position(|b| b["type"] == "thinking" && b["signature"] == blob);
            let tool_use = blocks.iter().position(|b| b["type"] == "tool_use");
            match (thinking, tool_use) {
                (Some(0), Some(call)) if call > 0 => {
                    if blocks[0]["thinking"] != fixture.expect.reasoning {
                        return Err(format!(
                            "the thinking text changed: {}",
                            blocks[0]["thinking"]
                        ));
                    }
                    Ok(())
                }
                other => Err(format!(
                    "thinking block / tool_use at {other:?} in {blocks:?}"
                )),
            }
        }
        // A reasoning item with the encrypted content, directly ahead of the
        // function_call item.
        Protocol::OpenaiResponses => {
            let items = body["input"].as_array().ok_or("no input")?;
            let at = items
                .iter()
                .position(|item| item["type"] == "reasoning" && item["encrypted_content"] == blob)
                .ok_or_else(|| format!("no reasoning item carries the blob: {items:?}"))?;
            if items
                .get(at + 1)
                .is_none_or(|next| next["type"] != "function_call")
            {
                return Err("the reasoning item is not directly ahead of the function_call".into());
            }
            Ok(())
        }
        // On the functionCall part itself.
        Protocol::Gemini => {
            let parts = body["contents"][1]["parts"]
                .as_array()
                .ok_or("no model parts")?;
            let call = parts
                .iter()
                .find(|part| part.get("functionCall").is_some())
                .ok_or("no functionCall part")?;
            if call["thoughtSignature"] != blob {
                return Err(format!(
                    "the functionCall part is signed with {}",
                    call["thoughtSignature"]
                ));
            }
            Ok(())
        }
        // Chat relays sign the reasoning detail; Google's compatible
        // endpoint signs the tool call.
        Protocol::OpenaiChat => {
            let message = &body["messages"][1];
            if fixture.name == "signed_tool_call" {
                let signature =
                    &message["tool_calls"][0]["extra_content"]["google"]["thought_signature"];
                if signature != blob {
                    return Err(format!("the tool call is signed with {signature}"));
                }
                return Ok(());
            }
            let details = message["reasoning_details"]
                .as_array()
                .ok_or_else(|| format!("no reasoning_details in {message}"))?;
            if !details.iter().any(|detail| detail["signature"] == blob) {
                return Err(format!("no reasoning detail carries the blob: {details:?}"));
            }
            if message["reasoning_content"] != fixture.expect.reasoning {
                return Err(format!(
                    "the reasoning text changed: {}",
                    message["reasoning_content"]
                ));
            }
            Ok(())
        }
    }
}

fn run_pair(client: Protocol, upstream: Protocol) {
    let mut failures = Failures::default();
    let fixtures: Vec<UpstreamResponse> = scenarios::responses(upstream)
        .into_iter()
        .filter(|fixture| matches!(fixture.name, "reasoning_tool" | "signed_tool_call"))
        .collect();
    assert!(!fixtures.is_empty());
    for fixture in &fixtures {
        let context = fixture.name;
        let blob = fixture.expect.blobs[0];

        // Turn 1: the upstream's answer, as the client receives it.
        let request1 = turn1(client);
        let decoded = codec(upstream)
            .decode_response(fixture.json.as_ref().expect("a complete body"))
            .expect("turn 1 decodes");
        let answer = codec(client)
            .encode_response(&decoded, &client_ctx(client, &request1))
            .expect("turn 1 translates");
        failures.report(context, validate_response(client, &answer));
        // A call signature for a client without a field for it: on the
        // carrier block directly ahead of the call, and nowhere else.
        let carrier = (blob_on_call(upstream, fixture) && !has_call_slot(client))
            .then(|| client_call_blob(client, upstream, blob));
        if let Some(carrier) = &carrier {
            if let Err(why) = carrier_ahead_of_call(client, &answer, carrier) {
                failures.push(context, why);
            }
            failures.check(
                context,
                !answer.to_string().replace(carrier, "").contains(blob),
                format!("the call signature is shown to the {client} client outside its carrier"),
            );
        }

        // Turn 2, back to the same upstream.
        let request2 = turn2(client, &answer);

        // The tool-call id is what the gateway keys remembered signatures on:
        // the client has to send back the one the upstream's answer carried.
        let issued: Vec<&str> = decoded.tool_calls().map(|call| call.id.as_str()).collect();
        match decode_request(client, &request2) {
            Ok(replayed) => {
                let returned: Vec<&str> = replayed
                    .messages
                    .iter()
                    .flat_map(Message::tool_calls)
                    .map(|call| call.id.as_str())
                    .collect();
                failures.check(
                    context,
                    !issued.is_empty() && issued == returned,
                    format!("tool-call ids {issued:?} came back from the client as {returned:?}"),
                );
            }
            Err(error) => failures.push(context, format!("turn 2 does not decode: {error}")),
        }

        let caps = known_caps(upstream);
        match translate_request(client, upstream, &request2, &caps.ctx()) {
            Ok(body) => {
                failures.report(
                    &format!("{context} -> {}", short(upstream)),
                    validate_translated_request(client, upstream, &body),
                );
                if let Err(why) = in_native_position(upstream, fixture, &body, blob) {
                    failures.push(
                        context,
                        format!(
                            "the blob did not return to {upstream} in its native position: {why}"
                        ),
                    );
                }
                // The carrier is this gateway's own construct: none of it
                // may reach the upstream.
                failures.check(
                    context,
                    !body.to_string().contains("call:"),
                    format!("a carrier marker is sent to {upstream}"),
                );
                if upstream == ANTHROPIC {
                    // The turn in progress opens with its signed thinking
                    // block again, so manual thinking stays on.
                    failures.check(
                        context,
                        body["thinking"]["type"] == "enabled",
                        format!(
                            "thinking was switched off for the tool loop: {}",
                            body["thinking"]
                        ),
                    );
                }
            }
            Err(error) => failures.push(
                context,
                format!("turn 2 does not translate for {upstream}: {error}"),
            ),
        }

        // Turn 2 from a client that did not echo the carrier: the call comes
        // back unsigned, in a form the upstream accepts.
        if let Some(carrier) = &carrier {
            let context = format!("{context} (carrier not echoed)");
            let stripped = turn2(client, &without_carrier(client, &answer, carrier));
            match translate_request(client, upstream, &stripped, &caps.ctx()) {
                Ok(body) => {
                    failures.report(
                        &context,
                        validate_translated_request(client, upstream, &body),
                    );
                    if let Err(why) = unsigned_call(upstream, &body) {
                        failures.push(&context, why);
                    }
                }
                Err(error) => failures.push(
                    &context,
                    format!("turn 2 does not translate for {upstream}: {error}"),
                ),
            }
        }

        // Turn 2, failing over to every other upstream.
        for other in Protocol::ALL {
            if other == upstream {
                continue;
            }
            let context = format!("{context} -> {}", short(other));
            let caps = known_caps(other);
            match translate_request(client, other, &request2, &caps.ctx()) {
                Ok(body) => {
                    failures.report(&context, validate_translated_request(client, other, &body));
                    failures.check(
                        &context,
                        !body.to_string().contains(blob),
                        format!("a blob issued by {upstream} is sent to {other}"),
                    );
                    // The conversation itself must survive the failover.
                    let wire = body.to_string();
                    failures.check(
                        &context,
                        wire.contains(TOOL_OUTPUT),
                        "the tool result is missing",
                    );
                    failures.check(
                        &context,
                        wire.contains("get_weather"),
                        "the tool call is missing",
                    );
                }
                Err(error) => {
                    failures.push(&context, format!("turn 2 does not translate: {error}"))
                }
            }
        }
    }
    failures.finish(&format!(
        "{} client, {} upstream",
        short(client),
        short(upstream)
    ));
}

macro_rules! pairs {
    ($($name:ident: $client:expr => $upstream:expr;)*) => {
        $(
            #[test]
            fn $name() {
                run_pair($client, $upstream);
            }
        )*
    };
}

pairs! {
    chat_via_chat: CHAT => CHAT;
    chat_via_responses: CHAT => RESPONSES;
    chat_via_anthropic: CHAT => ANTHROPIC;
    chat_via_gemini: CHAT => GEMINI;
    responses_via_chat: RESPONSES => CHAT;
    responses_via_responses: RESPONSES => RESPONSES;
    responses_via_anthropic: RESPONSES => ANTHROPIC;
    responses_via_gemini: RESPONSES => GEMINI;
    anthropic_via_chat: ANTHROPIC => CHAT;
    anthropic_via_responses: ANTHROPIC => RESPONSES;
    anthropic_via_anthropic: ANTHROPIC => ANTHROPIC;
    anthropic_via_gemini: ANTHROPIC => GEMINI;
    gemini_via_chat: GEMINI => CHAT;
    gemini_via_responses: GEMINI => RESPONSES;
    gemini_via_anthropic: GEMINI => ANTHROPIC;
    gemini_via_gemini: GEMINI => GEMINI;
}
