//! Matrix (b): every canned upstream response, for every ordered pair of
//! client protocol `C` and upstream protocol `U`, goes `U.decode_response` →
//! `C.encode_response` and must come out as a body a `C` client accepts,
//! with the content preserved: text, reasoning, tool calls (names equal,
//! arguments JSON-equal), the finish reason per the mapping tables, usage
//! under `C`'s convention, and opaque blobs in `C`'s slot for them.

mod support;

use serde_json::Value;
use support::harness::{
    ANTHROPIC, CHAT, Failures, GEMINI, RESPONSES, client_ctx, no_panic, short, validate_response,
};
use support::scenarios::{self, UpstreamResponse};
use support::views::{
    carrier_ahead_of_call, client_blob, client_call_blob, expected_finish, has_call_slot,
    summarize, usage_mismatches, view,
};
use switchyard_codecs::codec;
use switchyard_core::Protocol;
use switchyard_core::ir::{Part, Response};

/// Which canonical slot a blob of a decoded response sits in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    /// On a reasoning part: every client protocol has a place for it.
    Reasoning,
    /// On a tool call: Chat (`extra_content.google.thought_signature`) and
    /// Gemini (`thoughtSignature`) have a field for it. Anthropic and
    /// Responses have none; for those clients the signature rides on a
    /// text-less reasoning block directly ahead of the call, marked as a
    /// call signature (`views::client_call_blob`), and
    /// `matrix_round_trip.rs` checks that it finds its way back onto the
    /// call.
    Call,
    /// On answer text: only Gemini has a place for it.
    Text,
}

fn blob_slots(response: &Response) -> Vec<(Slot, String)> {
    let mut out = Vec::new();
    for part in &response.parts {
        match part {
            Part::Reasoning(r) => out.extend(
                r.signature
                    .iter()
                    .map(|s| (Slot::Reasoning, s.data.clone())),
            ),
            Part::ToolCall(c) => {
                out.extend(c.signature.iter().map(|s| (Slot::Call, s.data.clone())))
            }
            Part::Text(t) => out.extend(t.signature.iter().map(|s| (Slot::Text, s.data.clone()))),
            _ => {}
        }
    }
    out
}

/// Checks that the fixture decodes to what it says it contains: a mismatch
/// here is a bug in the upstream side of `U`'s codec (or in the fixture).
fn check_decoded(
    fixture: &UpstreamResponse,
    response: &Response,
    failures: &mut Failures,
    context: &str,
) {
    let expect = &fixture.expect;
    let summary = summarize(response);
    failures.check(
        context,
        summary.text == expect.text,
        format!(
            "decoded text {:?}, expected {:?}",
            summary.text, expect.text
        ),
    );
    failures.check(
        context,
        summary.reasoning == expect.reasoning,
        format!(
            "decoded reasoning {:?}, expected {:?}",
            summary.reasoning, expect.reasoning
        ),
    );
    failures.check(
        context,
        summary.refusal == expect.refusal,
        format!(
            "decoded refusal {:?}, expected {:?}",
            summary.refusal, expect.refusal
        ),
    );
    let calls: Vec<(String, Value)> = summary
        .calls
        .iter()
        .map(|(_, name, args)| (name.clone(), args.clone()))
        .collect();
    let expected_calls: Vec<(String, Value)> = expect
        .calls
        .iter()
        .map(|c| (c.name.to_string(), c.arguments.clone()))
        .collect();
    failures.check(
        context,
        calls == expected_calls,
        format!("decoded calls {calls:?}, expected {expected_calls:?}"),
    );
    failures.check(
        context,
        summary.finish == expect.finish,
        format!(
            "decoded finish {:?}, expected {:?}",
            summary.finish, expect.finish
        ),
    );
    if let Some(usage) = expect.usage {
        failures.check(
            context,
            summary.usage == usage,
            format!("decoded usage {:?}, expected {usage:?}", summary.usage),
        );
    }
    let blobs: Vec<String> = blob_slots(response)
        .into_iter()
        .map(|(_, blob)| blob)
        .collect();
    for blob in &expect.blobs {
        failures.check(
            context,
            blobs.iter().any(|b| b == blob),
            format!("blob {blob} was not decoded"),
        );
    }
}

fn run_pair(client: Protocol, upstream: Protocol) {
    let mut failures = Failures::default();
    let request = scenarios::tool_request(client);
    let ctx = client_ctx(client, &request);
    let fixtures = scenarios::responses(upstream);
    assert!(fixtures.len() >= 10, "the response library shrank");
    for fixture in &fixtures {
        let Some(body) = &fixture.json else {
            continue;
        };
        let context = fixture.name;
        let decoded = match no_panic(context, || codec(upstream).decode_response(body)) {
            Err(panic) => {
                failures.push(context, panic);
                continue;
            }
            Ok(Err(error)) => {
                failures.push(
                    context,
                    format!("{upstream} decode_response failed: {error}"),
                );
                continue;
            }
            Ok(Ok(response)) => response,
        };
        check_decoded(fixture, &decoded, &mut failures, context);

        let encoded = match no_panic(context, || codec(client).encode_response(&decoded, &ctx)) {
            Err(panic) => {
                failures.push(context, panic);
                continue;
            }
            Ok(Err(error)) => {
                failures.push(context, format!("{client} encode_response failed: {error}"));
                continue;
            }
            Ok(Ok(body)) => body,
        };
        failures.report(context, validate_response(client, &encoded));

        let expect = &fixture.expect;
        let seen = view(client, &encoded);
        // Anthropic and Gemini have no refusal slot: a refusal is answer text.
        let (text, refusal) = match client {
            Protocol::OpenaiChat | Protocol::OpenaiResponses => {
                (expect.text.to_string(), expect.refusal.to_string())
            }
            _ => (format!("{}{}", expect.text, expect.refusal), String::new()),
        };
        failures.check(
            context,
            seen.text == text,
            format!("text {:?}, expected {text:?}", seen.text),
        );
        failures.check(
            context,
            seen.refusal == refusal,
            format!("refusal {:?}, expected {refusal:?}", seen.refusal),
        );
        failures.check(
            context,
            seen.reasoning == expect.reasoning,
            format!(
                "reasoning {:?}, expected {:?}",
                seen.reasoning, expect.reasoning
            ),
        );
        let expected_calls: Vec<(String, Value)> = expect
            .calls
            .iter()
            .map(|c| (c.name.to_string(), c.arguments.clone()))
            .collect();
        failures.check(
            context,
            seen.calls == expected_calls,
            format!("tool calls {:?}, expected {expected_calls:?}", seen.calls),
        );
        let stop_sequence = fixture.name == "stop_sequence";
        let finish = expected_finish(client, expect, stop_sequence);
        failures.check(
            context,
            seen.finish == finish,
            format!("finish {:?}, expected {finish:?}", seen.finish),
        );
        if client == ANTHROPIC {
            let expected = stop_sequence.then(|| "seven".to_string());
            failures.check(
                context,
                seen.stop_sequence == expected,
                format!(
                    "stop_sequence {:?}, expected {expected:?}",
                    seen.stop_sequence
                ),
            );
        }
        if let Some(usage) = &expect.usage {
            for mismatch in usage_mismatches(client, &encoded, usage) {
                failures.push(context, mismatch);
            }
        }

        let wire = encoded.to_string();
        for (slot, blob) in blob_slots(&decoded) {
            let has_slot = match slot {
                Slot::Reasoning | Slot::Call => true,
                Slot::Text => client == GEMINI,
            };
            let expected = match slot {
                Slot::Call => client_call_blob(client, upstream, &blob),
                _ => client_blob(client, upstream, &blob),
            };
            if has_slot {
                failures.check(
                    context,
                    wire.contains(&expected),
                    format!("{slot:?} blob should reach the client as `{expected}`"),
                );
            }
            if slot == Slot::Call && !has_call_slot(client) {
                // No field on the call itself: the carrier block sits
                // directly ahead of the call it signs.
                if let Err(why) = carrier_ahead_of_call(client, &encoded, &expected) {
                    failures.push(context, why);
                }
            }
            if client != upstream {
                // Whatever reaches a client of another protocol is tagged
                // with its origin, never bare.
                let untagged = wire.replace(&expected, "");
                failures.check(
                    context,
                    !untagged.contains(&blob),
                    format!("{slot:?} blob of {upstream} reaches the {client} client untagged"),
                );
            }
        }
    }
    failures.finish(&format!("{} <- {}", short(client), short(upstream)));
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
    chat_from_chat: CHAT => CHAT;
    chat_from_responses: CHAT => RESPONSES;
    chat_from_anthropic: CHAT => ANTHROPIC;
    chat_from_gemini: CHAT => GEMINI;
    responses_from_chat: RESPONSES => CHAT;
    responses_from_responses: RESPONSES => RESPONSES;
    responses_from_anthropic: RESPONSES => ANTHROPIC;
    responses_from_gemini: RESPONSES => GEMINI;
    anthropic_from_chat: ANTHROPIC => CHAT;
    anthropic_from_responses: ANTHROPIC => RESPONSES;
    anthropic_from_anthropic: ANTHROPIC => ANTHROPIC;
    anthropic_from_gemini: ANTHROPIC => GEMINI;
    gemini_from_chat: GEMINI => CHAT;
    gemini_from_responses: GEMINI => RESPONSES;
    gemini_from_anthropic: GEMINI => ANTHROPIC;
    gemini_from_gemini: GEMINI => GEMINI;
}
