//! Regression tests from the adversarial review of secret redaction
//! (`redact` module, body capture and the log capture layer), through the
//! crate's public API only.
//!
//! Each test failed against the first implementation; the comments describe
//! the defect that was fixed. Sources: track item 6/7 ("redact obvious
//! secrets", "reuse the redaction helper"), reference notes 13 §5.6 and §6.6,
//! and the crate's own `redact` module docs.

use std::collections::BTreeMap;
use switchyard_core::config::RequestLogMode;
use switchyard_telemetry::{
    BodyStore, CapturedBodies, DEFAULT_MAX_BODY_BYTES, EventBus, LogBuffer, capture_layer,
    redact_body, redact_header_value, redact_headers, redact_json_secrets, redact_text,
};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt;

/// 2026-10-02T00:00:00Z.
const T0: i64 = 1_790_899_200_000;
const KEY: &str = "sk-proj-abcdefghijklmnopqrstuvwxyz";
const GOOGLE_KEY: &str = "AIzaSyA-0123456789abcdefghijklmnopqrstu";

// ---------------------------------------------------------------------------
// R1. A JSON body is only redacted by KEY NAME. A secret inside a string
// value (an upstream error echoing the key, an `Authorization: Bearer …`
// line inside a message, a JSON-encoded tool argument string) is stored in
// the clear, although the very same text in a non-JSON body (SSE) is masked
// by `redact_text`. The crate's own unit test
// `bodies_are_redacted_before_they_are_stored` only covers the SSE spelling.
// ---------------------------------------------------------------------------

#[test]
fn r1_json_body_string_values_are_scanned_for_key_shapes() {
    // What an OpenAI-compatible upstream answers with on a bad key
    // (non-streaming, so the body is one JSON document).
    let body = format!(
        r#"{{"error":{{"message":"Incorrect API key provided: {KEY}. You can find your API key at https://example.com/keys","type":"invalid_request_error","code":"invalid_api_key"}}}}"#
    );
    let out = redact_body(&body);
    assert!(
        !out.contains(KEY),
        "upstream key survived in a JSON body: {out}"
    );

    // The same message wrapped as an SSE data line IS redacted today, which
    // shows the intent: both spellings must be safe.
    let sse = format!("data: {body}\n\n");
    assert!(!redact_body(&sse).contains(KEY));
}

#[test]
fn r1_json_body_bearer_and_nested_json_strings() {
    // A Bearer credential inside a string value under a non-secret key.
    let body = format!(
        r#"{{"tools":[{{"type":"mcp","server_url":"https://example.com/mcp","headers":{{"X-Upstream-Auth":"Bearer {KEY}"}}}}]}}"#
    );
    let out = redact_body(&body);
    assert!(!out.contains(KEY), "{out}");

    // Chat Completions tool-call arguments are a JSON document encoded as a
    // string; a key inside it is as visible as anywhere else.
    let body = format!(
        r#"{{"messages":[{{"role":"assistant","tool_calls":[{{"id":"call_1","type":"function","function":{{"name":"login","arguments":"{{\"api_key\":\"{KEY}\"}}"}}}}]}}]}}"#
    );
    let out = redact_body(&body);
    assert!(!out.contains(KEY), "{out}");
}

#[test]
fn r1_captured_json_error_body_does_not_reach_the_disk_with_the_key() {
    let tmp = tempfile::tempdir().unwrap();
    let store = BodyStore::new(
        Some(tmp.path()),
        RequestLogMode::All,
        DEFAULT_MAX_BODY_BYTES,
    );
    let bodies = CapturedBodies {
        upstream_response: Some(format!(
            r#"{{"error":{{"message":"Incorrect API key provided: {KEY}","type":"invalid_request_error"}}}}"#
        )),
        ..CapturedBodies::default()
    };
    assert!(store.capture_at("r1", T0, true, bodies).unwrap());
    let text = std::fs::read_to_string(tmp.path().join("requests/2026-10-02/r1.json")).unwrap();
    assert!(!text.contains(KEY), "{text}");
}

// ---------------------------------------------------------------------------
// R2. `redact_text`: after an `authorization:` name the scheme word is
// skipped and the credential that follows is only masked when it is
// `Bearer` + 16 or more characters. `Basic <base64(user:password)>` and a
// short bearer token (Switchyard client keys can be any length) stay in the
// clear. Notes 13 §6.6: "replace `Bearer|Basic <x>` with `<scheme> [REDACTED]`".
// ---------------------------------------------------------------------------

/// base64("user:hunter2-password")
const BASIC: &str = "dXNlcjpodW50ZXIyLXBhc3N3b3Jk";

#[test]
fn r2_text_basic_credentials_are_redacted() {
    for text in [
        format!("Proxy-Authorization: Basic {BASIC}"),
        format!("sent authorization: Basic {BASIC} to the proxy"),
        format!(r#"headers={{"proxy-authorization": "Basic {BASIC}"}}"#),
    ] {
        let out = redact_text(&text);
        assert!(!out.contains(BASIC), "basic credentials survived: {out}");
    }
}

#[test]
fn r2_text_short_bearer_token_after_authorization_is_redacted() {
    // 12 characters: shorter than the 16 the BEARER pattern wants, and the
    // `authorization:` assignment pattern stops at the scheme word.
    let out = redact_text("Authorization: Bearer abc123def456");
    assert!(!out.contains("abc123def456"), "{out}");
}

#[test]
fn r2_log_layer_redacts_basic_credentials() {
    let buffer = LogBuffer::default();
    let subscriber = Registry::default().with(capture_layer(buffer.clone(), EventBus::default()));
    tracing::subscriber::with_default(subscriber, || {
        tracing::warn!(
            headers = %format!(r#"{{"proxy-authorization": "Basic {BASIC}"}}"#),
            "proxy rejected Proxy-Authorization: Basic {BASIC}"
        );
    });
    let line = &buffer.query(1, None, None, None)[0];
    let text = serde_json::to_string(line.as_ref()).unwrap();
    assert!(!text.contains(BASIC), "{text}");
}

/// Basic credentials are a password. The module doc says values that are
/// secret in their entirety "are replaced by `[redacted]`", but the header
/// path masks them like an API key (prefix + last four characters). The last
/// four base64 characters decode to the last three bytes of the password; for
/// a short credential that is the whole password.
#[test]
fn r2_basic_header_does_not_keep_the_tail_of_the_password() {
    // base64("admin:pw") = "YWRtaW46cHc="; "cHc=" decodes to "pw".
    let out = redact_header_value("proxy-authorization", "Basic YWRtaW46cHc=");
    assert!(out.starts_with("Basic "), "{out}");
    assert!(
        !out.contains("cHc="),
        "the base64 tail (= the password) is still readable: {out}"
    );
}

// ---------------------------------------------------------------------------
// R3. `redact_text`: credentials in the userinfo part of a URL
// (`http://user:password@proxy:8080`, the form `upstream.proxy` takes) are
// not redacted. Notes 13 §6.6: "replace URL userinfo with `[REDACTED]@`".
// ---------------------------------------------------------------------------

#[test]
fn r3_text_url_userinfo_is_redacted() {
    let out = redact_text(
        "proxy connect failed: http://alice:hunter2-very-secret@proxy.example:8080 refused",
    );
    assert!(!out.contains("hunter2-very-secret"), "{out}");
    assert!(
        out.contains("proxy.example:8080"),
        "host must stay readable: {out}"
    );

    let buffer = LogBuffer::default();
    let subscriber = Registry::default().with(capture_layer(buffer.clone(), EventBus::default()));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(
            proxy = "socks5://bob:s3cr3t-pa55word@10.0.0.1:1080",
            "using proxy"
        );
    });
    let line = &buffer.query(1, None, None, None)[0];
    let text = serde_json::to_string(line.as_ref()).unwrap();
    assert!(!text.contains("s3cr3t-pa55word"), "{text}");
}

// ---------------------------------------------------------------------------
// R4. Header values are only redacted when the header NAME looks secret.
// A key travelling in the value of an innocently named header is stored
// verbatim: the original request URI a reverse proxy forwards
// (`?key=` is one of the gateway's own auth locations, DESIGN §9), a
// referer, or the WebSocket sub-protocol OpenAI browser clients put the key
// in. Track item 6 lists "key= query values" among the rules.
// ---------------------------------------------------------------------------

#[test]
fn r4_header_values_carrying_a_key_are_redacted() {
    let headers = redact_headers([
        (
            "x-original-uri",
            format!("/v1beta/models/gemini-2.5-pro:generateContent?key={GOOGLE_KEY}"),
        ),
        (
            "sec-websocket-protocol",
            format!("realtime, openai-insecure-api-key.{KEY}, openai-beta.realtime-v1"),
        ),
        (
            "referer",
            "https://app.example/chat?access_token=abcdef0123456789abcdef".to_string(),
        ),
    ]);
    assert!(
        !headers["x-original-uri"].contains(GOOGLE_KEY),
        "{}",
        headers["x-original-uri"]
    );
    assert!(
        !headers["sec-websocket-protocol"].contains(KEY),
        "{}",
        headers["sec-websocket-protocol"]
    );
    assert!(
        !headers["referer"].contains("abcdef0123456789abcdef"),
        "{}",
        headers["referer"]
    );
}

#[test]
fn r4_body_store_rechecks_raw_header_values_too() {
    // `BodyStore::prepare` documents "a raw secret that slipped in is caught
    // here"; that only holds for secret-looking names.
    let tmp = tempfile::tempdir().unwrap();
    let store = BodyStore::new(
        Some(tmp.path()),
        RequestLogMode::All,
        DEFAULT_MAX_BODY_BYTES,
    );
    let prepared = store.prepare(CapturedBodies {
        client_headers: BTreeMap::from([(
            "x-forwarded-uri".to_string(),
            format!("/v1beta/models/g:streamGenerateContent?alt=sse&key={GOOGLE_KEY}"),
        )]),
        ..CapturedBodies::default()
    });
    assert!(
        !prepared.client_headers["x-forwarded-uri"].contains(GOOGLE_KEY),
        "{:?}",
        prepared.client_headers
    );
}

// ---------------------------------------------------------------------------
// R5. False positives that damage captured bodies.
// ---------------------------------------------------------------------------

/// Gemini log probabilities (`responseLogprobs: true`) use
/// `{"token": "...", "tokenId": n, "logProbability": x}`. The exemption for
/// model-output tokens only recognises OpenAI's `logprob` sibling, so every
/// Gemini token string is masked as if it were a credential.
#[test]
fn r5_gemini_logprob_tokens_are_model_output_not_secrets() {
    let mut body = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Hello world"}]},
            "avgLogprobs": -0.15,
            "logprobsResult": {
                "chosenCandidates": [
                    {"token": "Hello", "tokenId": 4521, "logProbability": -0.1},
                    {"token": " world", "tokenId": 991, "logProbability": -0.2}
                ],
                "topCandidates": [{
                    "candidates": [
                        {"token": "Hello", "tokenId": 4521, "logProbability": -0.1},
                        {"token": "Greetings", "tokenId": 77, "logProbability": -2.3}
                    ]
                }]
            }
        }]
    });
    let before = body.clone();
    let changed = redact_json_secrets(&mut body);
    assert_eq!(body, before, "Gemini logprob tokens were rewritten");
    assert!(!changed);
}

/// In a non-JSON body (SSE) the `name: value` rule also fires on JSON
/// literals: `"api_key":null` becomes `"api_key":••••`, which is no longer
/// JSON and hides nothing (there was no secret).
#[test]
fn r5_text_rule_leaves_json_literals_alone() {
    let line = r#"data: {"api_key":null,"password":false,"authorization":null,"x":1}"#;
    assert_eq!(redact_text(line), line);
}
