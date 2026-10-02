//! Review finding: upstream error text that quotes an API key reaches the
//! client through three of the four codecs.
//!
//! `ApiError::message` "never contains credentials" (`core::error`) and the
//! quality bar is "no secrets in logs or errors". Compatible servers and
//! relays do echo the key they were called with ("Incorrect API key
//! provided: sk-…"). The key in question is the *operator's* provider key,
//! and the message is rendered to the gateway's client: in a translated
//! stream the codec's `StreamEvent::Error` goes straight to the client's
//! stream encoder. The Responses codec scrubs such text (`[REDACTED]`); the
//! Chat, Anthropic and Gemini codecs pass it through. These tests currently
//! FAIL for those three.

mod support;

use support::harness::{ANTHROPIC, CHAT, GEMINI, RESPONSES, decode_stream, parse_sse, short};
use switchyard_codecs::codec;
use switchyard_core::Protocol;
use switchyard_core::stream::StreamEvent;

const OPENAI_KEY: &str = "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCD";
const ANTHROPIC_KEY: &str = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCD-xyzAA";
const GEMINI_KEY: &str = "AIzaSyA1234567890abcdefghijklmnopqrstuvw";

fn key_of(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenaiChat | Protocol::OpenaiResponses => OPENAI_KEY,
        Protocol::Anthropic => ANTHROPIC_KEY,
        Protocol::Gemini => GEMINI_KEY,
    }
}

fn error_body(protocol: Protocol) -> String {
    let key = key_of(protocol);
    match protocol {
        Protocol::OpenaiChat | Protocol::OpenaiResponses => format!(
            r#"{{"error":{{"message":"Incorrect API key provided: {key}. You can find your API key at https://platform.openai.com/account/api-keys.","type":"invalid_request_error","param":null,"code":"invalid_api_key"}}}}"#
        ),
        Protocol::Anthropic => format!(
            r#"{{"type":"error","error":{{"type":"authentication_error","message":"invalid x-api-key: {key}"}}}}"#
        ),
        Protocol::Gemini => format!(
            r#"{{"error":{{"code":400,"message":"API key not valid: {key}. Please pass a valid API key.","status":"INVALID_ARGUMENT"}}}}"#
        ),
    }
}

fn error_stream(protocol: Protocol) -> String {
    let key = key_of(protocol);
    match protocol {
        Protocol::OpenaiChat => format!(
            "data: {{\"error\":{{\"message\":\"upstream rejected key {key}\",\"type\":\"server_error\"}}}}\n\n"
        ),
        Protocol::OpenaiResponses => format!(
            "event: error\ndata: {{\"type\":\"error\",\"code\":\"server_error\",\"message\":\"upstream rejected key {key}\",\"param\":null,\"sequence_number\":1}}\n\n"
        ),
        Protocol::Anthropic => format!(
            "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":\"upstream rejected key {key}\"}}}}\n\n"
        ),
        Protocol::Gemini => format!(
            "data: {{\"error\":{{\"code\":500,\"message\":\"upstream rejected key {key}\",\"status\":\"INTERNAL\"}}}}\n\n"
        ),
    }
}

#[test]
fn decode_error_does_not_repeat_an_api_key() {
    let mut failures = Vec::new();
    for protocol in [RESPONSES, CHAT, ANTHROPIC, GEMINI] {
        let info = codec(protocol).decode_error(401, error_body(protocol).as_bytes());
        if info.message.contains(key_of(protocol)) {
            failures.push(format!(
                "{}: decode_error message quotes the key: {}",
                short(protocol),
                info.message
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn an_in_stream_error_does_not_repeat_an_api_key() {
    let mut failures = Vec::new();
    for protocol in [RESPONSES, CHAT, ANTHROPIC, GEMINI] {
        let events = decode_stream(protocol, &parse_sse(&error_stream(protocol)));
        let Some(StreamEvent::Error(error)) = events.last() else {
            panic!(
                "{}: the stream should end with an error: {events:?}",
                short(protocol)
            );
        };
        if error.message.contains(key_of(protocol)) {
            failures.push(format!(
                "{}: the StreamEvent::Error sent on to the client quotes the key: {}",
                short(protocol),
                error.message
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
