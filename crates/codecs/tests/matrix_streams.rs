//! Matrix (c): every canned upstream stream transcript, for every ordered
//! pair of client protocol `C` and upstream protocol `U`:
//!
//! ```text
//! SseParser → U.stream_decoder → stream::validate_sequence
//!           → C.stream_encoder → C's stream validator
//!           → C.stream_decoder → Accumulator
//! ```
//!
//! and the accumulated result must equal the non-stream translation of the
//! same response (`U.decode_response` → `C.encode_response` →
//! `C.decode_response`), so a client gets the same answer whether or not it
//! streams.

mod support;

use support::harness::{
    ANTHROPIC, CHAT, Failures, GEMINI, RESPONSES, accumulate, client_ctx, decode_stream,
    encode_stream, no_panic, over_the_wire, parse_sse, short, validate_stream,
};
use support::scenarios::{self, StreamEnd, UpstreamResponse};
use support::views::{Summary, summarize};
use switchyard_codecs::codec;
use switchyard_core::Protocol;
use switchyard_core::ir::FinishReason;
use switchyard_core::stream::StreamEvent;

/// What a client's stream says, once decoded again, when the upstream's
/// stream simply stopped (`Finish { reason: Error }` from the upstream
/// decoder's `finish()`).
///
/// * Chat has no in-band "failed" for a finished stream: the cut is reported
///   as `finish_reason: "length"` — the answer is incomplete (documented on
///   the Chat codec);
/// * Anthropic ends with an `error` event, Responses with `response.failed`;
/// * Gemini reports `finishReason: "OTHER"`.
fn truncated_finish(client: Protocol) -> FinishReason {
    match client {
        Protocol::OpenaiChat => FinishReason::Length,
        Protocol::OpenaiResponses | Protocol::Anthropic => FinishReason::Error,
        Protocol::Gemini => FinishReason::Other("OTHER".to_string()),
    }
}

/// Stream and non-stream forms of one fixture may legitimately differ in
/// where an opaque blob sits.
fn blobs_comparable(upstream: Protocol, fixture: &UpstreamResponse) -> bool {
    // Gemini: without function calls the signature is on the answer text in
    // a complete response, and on a text-less part of its own in a stream
    // (notes 15 §6.5). Only the second form has a slot outside Gemini.
    !(upstream == GEMINI && fixture.name == "reasoning_text")
}

fn compare(
    stream: &Summary,
    complete: &Summary,
    blobs: bool,
    failures: &mut Failures,
    context: &str,
) {
    failures.check(
        context,
        stream.text == complete.text,
        format!(
            "stream text {:?} != complete text {:?}",
            stream.text, complete.text
        ),
    );
    failures.check(
        context,
        stream.reasoning == complete.reasoning,
        format!(
            "stream reasoning {:?} != complete reasoning {:?}",
            stream.reasoning, complete.reasoning
        ),
    );
    failures.check(
        context,
        stream.refusal == complete.refusal,
        format!(
            "stream refusal {:?} != complete refusal {:?}",
            stream.refusal, complete.refusal
        ),
    );
    failures.check(
        context,
        stream.calls == complete.calls,
        format!(
            "stream calls {:?} != complete calls {:?}",
            stream.calls, complete.calls
        ),
    );
    failures.check(
        context,
        stream.finish == complete.finish,
        format!(
            "stream finish {:?} != complete finish {:?}",
            stream.finish, complete.finish
        ),
    );
    failures.check(
        context,
        stream.usage == complete.usage,
        format!(
            "stream usage {:?} != complete usage {:?}",
            stream.usage, complete.usage
        ),
    );
    if blobs {
        failures.check(
            context,
            stream.blobs == complete.blobs,
            format!(
                "stream blobs {:?} != complete blobs {:?}",
                stream.blobs, complete.blobs
            ),
        );
    }
}

fn run_pair(client: Protocol, upstream: Protocol) {
    let mut failures = Failures::default();
    let request = scenarios::tool_request(client);
    let ctx = client_ctx(client, &request);
    for fixture in &scenarios::responses(upstream) {
        let Some(transcript) = fixture.sse else {
            continue;
        };
        let context = fixture.name;
        let expect = &fixture.expect;

        // Upstream wire → canonical events (the sequence contract is checked
        // inside `decode_stream`).
        let canonical = match no_panic(context, || decode_stream(upstream, &parse_sse(transcript)))
        {
            Ok(events) => events,
            Err(panic) => {
                failures.push(context, panic);
                continue;
            }
        };
        let upstream_summary = summarize(&accumulate(&canonical));
        failures.check(
            context,
            upstream_summary.text == expect.text,
            format!(
                "{upstream} stream decoded text {:?}, expected {:?}",
                upstream_summary.text, expect.text
            ),
        );
        failures.check(
            context,
            upstream_summary.reasoning == expect.reasoning,
            format!(
                "{upstream} stream decoded reasoning {:?}, expected {:?}",
                upstream_summary.reasoning, expect.reasoning
            ),
        );
        failures.check(
            context,
            upstream_summary.finish == expect.finish,
            format!(
                "{upstream} stream decoded finish {:?}, expected {:?}",
                upstream_summary.finish, expect.finish
            ),
        );
        if let Some(usage) = expect.usage {
            failures.check(
                context,
                upstream_summary.usage == usage,
                format!(
                    "{upstream} stream decoded usage {:?}, expected {usage:?}",
                    upstream_summary.usage
                ),
            );
        }
        match fixture.end {
            StreamEnd::Error => failures.check(
                context,
                matches!(canonical.last(), Some(StreamEvent::Error(_))),
                "an in-stream error must end the sequence with StreamEvent::Error",
            ),
            StreamEnd::Truncated => failures.check(
                context,
                matches!(
                    canonical.last(),
                    Some(StreamEvent::Finish {
                        reason: FinishReason::Error,
                        ..
                    })
                ),
                "a truncated stream must end with Finish { reason: Error }",
            ),
            StreamEnd::Complete => {}
        }

        // Canonical events → client wire, validated as the client's vendor
        // would send it.
        let wire = match no_panic(context, || {
            over_the_wire(&encode_stream(client, &ctx, &canonical))
        }) {
            Ok(wire) => wire,
            Err(panic) => {
                failures.push(context, panic);
                continue;
            }
        };
        failures.report(context, validate_stream(client, &wire));

        // Client wire → canonical again, as a client SDK would read it.
        let reread = match no_panic(context, || decode_stream(client, &wire)) {
            Ok(events) => events,
            Err(panic) => {
                failures.push(context, panic);
                continue;
            }
        };
        let stream_summary = summarize(&accumulate(&reread));

        match fixture.end {
            StreamEnd::Complete => {
                let Some(body) = &fixture.json else {
                    continue;
                };
                let complete = codec(upstream)
                    .decode_response(body)
                    .and_then(|response| codec(client).encode_response(&response, &ctx))
                    .and_then(|encoded| codec(client).decode_response(&encoded));
                match complete {
                    Ok(response) => compare(
                        &stream_summary,
                        &summarize(&response),
                        blobs_comparable(upstream, fixture),
                        &mut failures,
                        context,
                    ),
                    Err(error) => {
                        failures.push(context, format!("non-stream translation failed: {error}"))
                    }
                }
            }
            StreamEnd::Error => {
                failures.check(
                    context,
                    stream_summary.finish == FinishReason::Error,
                    format!(
                        "the client stream should end as failed, got {:?}",
                        stream_summary.finish
                    ),
                );
                failures.check(
                    context,
                    matches!(reread.last(), Some(StreamEvent::Error(_))),
                    "the client stream should carry the error to the client",
                );
                failures.check(
                    context,
                    stream_summary.text == expect.text,
                    format!(
                        "partial text {:?} lost, got {:?}",
                        expect.text, stream_summary.text
                    ),
                );
            }
            StreamEnd::Truncated => {
                let expected = truncated_finish(client);
                failures.check(
                    context,
                    stream_summary.finish == expected,
                    format!(
                        "a truncated upstream should reach the client as {expected:?}, got {:?}",
                        stream_summary.finish
                    ),
                );
                failures.check(
                    context,
                    stream_summary.text == expect.text,
                    format!(
                        "partial text {:?} lost, got {:?}",
                        expect.text, stream_summary.text
                    ),
                );
            }
        }
    }
    failures.finish(&format!(
        "{} <- {} (stream)",
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
