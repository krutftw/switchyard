//! Review finding: `Debug` output of `Target` and `BuiltRequest` prints the
//! values of configured headers in clear unless the header *name* happens to
//! contain one of a few substrings (`authorization`, `api-key`, `token`,
//! `secret`, ...). Provider `headers` are exactly where operators put
//! gateway credentials, and several widely used ones do not match.
//!
//! Track requirement: "Debug impls must never print secrets."
//!
//! These started as failing tests left by the adversarial review (findings
//! UP-2) and are kept as regression tests: the doc comment of each test
//! describes the defect as it was found, the assertions the behaviour that
//! is now implemented.

use http::HeaderMap;
use switchyard_core::Protocol;
use switchyard_core::config::{ProviderKind, ProxySetting};
use switchyard_upstream::{Auth, Operation, Target, build_request, build_ws_request};

/// Helicone's proxy credential (`Helicone-Auth: Bearer <key>`).
const HELICONE: &str = "sk-helicone-abcdefg-hijklmn-opqrstu-vwxyz12";
/// Azure API Management's credential header value.
const APIM: &str = "0123456789abcdef0123456789abcdef";
/// A generic "X-Auth-Key"-style credential.
const AUTH_KEY: &str = "auth-key-value-7f3c9d2e1b";

fn target() -> Target {
    Target {
        provider: "azure-apim".into(),
        kind: ProviderKind::OpenaiCompat,
        base_url: "https://gateway.example/openai/v1".into(),
        protocol: Protocol::OpenaiChat,
        model: "gpt-test".into(),
        auth: Auth::None,
        headers: vec![
            ("Helicone-Auth".into(), format!("Bearer {HELICONE}")),
            ("Ocp-Apim-Subscription-Key".into(), APIM.into()),
            ("X-Auth-Key".into(), AUTH_KEY.into()),
            // Harmless metadata next to them.
            ("X-Title".into(), "switchyard".into()),
        ],
        proxy: ProxySetting::Direct,
        project: String::new(),
        location: String::new(),
    }
}

fn assert_clean(what: &str, text: &str) {
    for secret in [HELICONE, APIM, AUTH_KEY] {
        assert!(
            !text.contains(secret),
            "{what} Debug output leaks a configured credential header:\n{text}"
        );
    }
}

#[test]
fn target_debug_does_not_print_credential_headers() {
    assert_clean("Target", &format!("{:?}", target()));
}

#[test]
fn built_request_debug_does_not_print_credential_headers() {
    let built = build_request(
        &target(),
        &Operation::Generate { stream: false },
        b"{}",
        &HeaderMap::new(),
    )
    .unwrap();
    // The headers are really sent...
    assert_eq!(built.header("ocp-apim-subscription-key"), Some(APIM));
    // ...but must not show up when the request is logged.
    assert_clean("BuiltRequest", &format!("{built:?}"));

    let ws = build_ws_request(&target(), "responses", &HeaderMap::new()).unwrap();
    assert_clean("WebSocket BuiltRequest", &format!("{ws:?}"));
}
