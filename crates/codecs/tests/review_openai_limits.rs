//! Review findings: a Messages client in front of an OpenAI upstream — the
//! most common cross-protocol traffic (Claude Code talking to a Chat or
//! Responses provider). Two request fields are forwarded past limits the
//! vendor enforces with a 400, which no failover can fix. These tests
//! currently FAIL.

mod support;

use serde_json::{Value, json};
use support::harness::{ANTHROPIC, CHAT, GEMINI, RESPONSES, decode_request};
use switchyard_codecs::codec;
use switchyard_core::codec::{MaxTokensField, Quirks};
use switchyard_core::reasoning::ModelThinking;
use switchyard_core::{Protocol, UpstreamCtx};

/// What Claude Code sends as `metadata.user_id`: user hash, account uuid and
/// session uuid, about 150 characters.
const CLAUDE_CODE_USER_ID: &str = "user_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef_account_11111111-2222-3333-4444-555555555555_session_66666666-7777-8888-9999-000000000000";

fn claude_code_request(max_tokens: u64) -> Value {
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": max_tokens,
        "stream": true,
        "system": [{"type": "text", "text": "You are a coding agent.", "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "List the files."}]}],
        "metadata": {"user_id": CLAUDE_CODE_USER_ID}
    })
}

fn encode(upstream: Protocol, body: &Value, ctx: &UpstreamCtx<'_>) -> Value {
    let mut request = decode_request(ANTHROPIC, body).expect("the request decodes");
    request.model = "upstream-model".to_string();
    codec(upstream)
        .encode_request(&request, ctx)
        .expect("the request encodes")
}

/// OpenAI's end-user identifier (`user`, and its successor
/// `safety_identifier`) is limited to 64 characters (API reference: "a
/// string that uniquely identifies each user, with a maximum length of 64
/// characters"); a longer one is answered with 400 `string too long`. The
/// reference does not map `metadata.user_id` to `user` at all (notes 06
/// §5 table: "`metadata.user_id` | — | not mapped"). The Chat and Responses
/// encoders forward Claude Code's ~150 character id verbatim, so every
/// request of such a client to an OpenAI upstream is refused. The Anthropic
/// encoder already guards its own limit the other way round
/// (`MAX_USER_ID_CHARS`).
#[test]
fn a_messages_client_user_id_is_not_sent_past_openais_64_characters() {
    assert!(CLAUDE_CODE_USER_ID.len() > 64);
    let body = claude_code_request(4096);
    let mut failures = Vec::new();
    for upstream in [CHAT, RESPONSES] {
        let encoded = encode(upstream, &body, &UpstreamCtx::default());
        for field in ["user", "safety_identifier"] {
            if let Some(value) = encoded.get(field).and_then(Value::as_str)
                && value.chars().count() > 64
            {
                failures.push(format!(
                    "{upstream}: `{field}` is {} characters long (limit 64): {value}",
                    value.chars().count()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The output-token limit a client states is written for *its* model. When
/// the gateway knows the target model's limit (`UpstreamCtx::max_output_tokens`)
/// the Anthropic, Gemini and Responses encoders lower the client's value to
/// it; the Chat encoder forwards it as written. Claude Code asks for
/// 32000/64000 tokens, so a Chat upstream model with a smaller limit
/// (gpt-4o: 16384, many compatible servers: 8192) answers every request
/// with 400 "max_tokens is too large".
#[test]
fn the_output_limit_is_lowered_to_the_target_models_limit_on_every_upstream() {
    let body = claude_code_request(64_000);
    let model_limit = 16_384u64;
    let mut failures = Vec::new();
    for upstream in [RESPONSES, ANTHROPIC, GEMINI, CHAT] {
        for field in [
            MaxTokensField::MaxCompletionTokens,
            MaxTokensField::MaxTokens,
        ] {
            let ctx = UpstreamCtx {
                thinking: ModelThinking::Unknown,
                max_output_tokens: Some(model_limit),
                quirks: Quirks {
                    max_tokens_field: field,
                    ..Quirks::default()
                },
            };
            let encoded = encode(upstream, &body, &ctx);
            let sent = match upstream {
                Protocol::OpenaiChat => encoded
                    .get("max_completion_tokens")
                    .or_else(|| encoded.get("max_tokens")),
                Protocol::OpenaiResponses => encoded.get("max_output_tokens"),
                Protocol::Anthropic => encoded.get("max_tokens"),
                Protocol::Gemini => encoded["generationConfig"].get("maxOutputTokens"),
            }
            .and_then(Value::as_u64);
            match sent {
                Some(sent) if sent <= model_limit => {}
                other => failures.push(format!(
                    "{upstream} ({field:?}): the body asks for {other:?} output tokens, \
                     the model's limit is {model_limit}"
                )),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
