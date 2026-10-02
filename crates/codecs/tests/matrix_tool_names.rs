//! Tool names across the seam, in both directions.
//!
//! The four protocols do not agree on what a tool may be called: OpenAI
//! takes `^[a-zA-Z0-9_-]{1,64}$`, Anthropic the same alphabet up to 128
//! characters, Gemini 64 characters that may include dots and colons. A
//! client declares its tools by its own vendor's rules, so for every ordered
//! pair of client protocol `C` and upstream protocol `U`:
//!
//! 1. the request `C` sends with a dotted name and a 100-character name is
//!    translated for `U`, and each tool must be declared there under exactly
//!    one name `U` accepts (`matrix_requests.rs` checks the rest of that
//!    body);
//! 2. the upstream then calls the tool by **the name it was given**, in a
//!    complete response and in a stream, and the client must be shown the
//!    call under **the name it declared**. A client that gets
//!    `mcp_files_read-file` back for its `mcp.files:read-file` has no such
//!    tool and the turn is lost.

mod support;

use serde_json::Value;
use support::harness::{
    ANTHROPIC, CHAT, Failures, GEMINI, RESPONSES, accumulate, client_ctx, declared_tools,
    decode_stream, encode_stream, known_caps, over_the_wire, parse_sse, same_tool, short,
    translate_request, validate_response, validate_stream, validate_translated_request,
};
use support::scenarios::{self, DOTTED_TOOL, LONG_TOOL, UpstreamResponse};
use support::views::view;
use switchyard_codecs::codec;
use switchyard_core::Protocol;

/// The name the canned tool-call responses of the scenario library call.
const CANNED_TOOL: &str = "get_weather";

/// The upstream's canned tool-call answer, calling `name` instead.
fn calling(fixture: &UpstreamResponse, name: &str) -> (Value, String) {
    let json = fixture
        .json
        .as_ref()
        .expect("a complete body")
        .to_string()
        .replace(CANNED_TOOL, name);
    let json: Value = serde_json::from_str(&json).expect("still JSON after renaming the tool");
    let sse = fixture
        .sse
        .expect("a transcript")
        .replace(CANNED_TOOL, name);
    (json, sse)
}

fn run_pair(client: Protocol, upstream: Protocol) {
    let mut failures = Failures::default();
    let scenario = scenarios::requests(client)
        .into_iter()
        .find(|scenario| scenario.name == "odd_tool_names")
        .expect("every client has the scenario");
    assert_eq!(scenario.tools, [DOTTED_TOOL, LONG_TOOL]);
    let fixture = scenarios::responses(upstream)
        .into_iter()
        .find(|fixture| fixture.name == "tool_call")
        .expect("every upstream has the fixture");

    let caps = known_caps(upstream);
    let body = translate_request(client, upstream, &scenario.body, &caps.ctx())
        .expect("the request translates");
    if client != upstream {
        failures.report(
            "request",
            validate_translated_request(client, upstream, &body),
        );
    }
    let declared = declared_tools(upstream, &body);
    let ctx = client_ctx(client, &scenario.body);

    for tool in &scenario.tools {
        let context = format!("{tool:.24}");
        // Gemini itself refuses a declaration longer than 64 characters, so
        // no Gemini client has such a tool to be called back by; the request
        // half of that case (the name is cut for every upstream) is covered
        // by `matrix_requests.rs`.
        if client == GEMINI && tool.len() > 64 {
            continue;
        }
        let given: Vec<&String> = declared
            .iter()
            .filter(|name| same_tool(tool, name))
            .collect();
        let [given] = given.as_slice() else {
            failures.push(
                &context,
                format!("declared upstream as {given:?} among {declared:?}"),
            );
            continue;
        };
        let (json, sse) = calling(&fixture, given);

        // Complete response.
        let response = codec(upstream)
            .decode_response(&json)
            .expect("the answer decodes");
        let encoded = codec(client)
            .encode_response(&response, &ctx)
            .expect("the answer encodes");
        failures.report(&context, validate_response(client, &encoded));
        let seen: Vec<String> = view(client, &encoded)
            .calls
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        failures.check(
            &context,
            seen == [tool.to_string()],
            format!("the upstream called `{given}`; the client is shown {seen:?} instead of the name it declared"),
        );

        // Stream.
        let events = decode_stream(upstream, &parse_sse(&sse));
        let wire = encode_stream(client, &ctx, &events);
        failures.report(&format!("{context}/stream"), validate_stream(client, &wire));
        let streamed = accumulate(&decode_stream(client, &over_the_wire(&wire)));
        let seen: Vec<&str> = streamed
            .tool_calls()
            .map(|call| call.name.as_str())
            .collect();
        failures.check(
            &format!("{context}/stream"),
            seen == [*tool],
            format!("the upstream called `{given}`; the client's stream shows {seen:?} instead of the name it declared"),
        );
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
