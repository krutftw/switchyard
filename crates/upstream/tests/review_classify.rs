//! Review findings in `classify()`.
//!
//! These started as failing tests left by the adversarial review (findings
//! UP-1, UP-3, UP-4, UP-5) and are kept as regression tests: the doc comment of each test
//! describes the defect as it was found, the assertions the behaviour that
//! is now implemented.

use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::json;
use switchyard_core::config::ProviderKind;
use switchyard_core::{FailureClass, Protocol};
use switchyard_upstream::classify;

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.append(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    h
}

const JSON: (&str, &str) = ("content-type", "application/json");

/// Notes 15 §6.6 (verified against Google's error table): HTTP 400 with
/// status `FAILED_PRECONDITION` means "free tier unavailable in country /
/// billing not enabled" — a property of the *key's project*, not of the
/// request. Another credential (a billed project) can serve the very same
/// request, so the failure must fail over and rest the credential.
///
/// The classifier only recognises the wording "location is not supported";
/// every other `FAILED_PRECONDITION` falls through to "400 = request fault",
/// which stops the attempt loop (DESIGN §8.5: `FailureClass::Request` → no
/// failover, no cooldown) and keeps the broken key in rotation.
#[test]
fn google_failed_precondition_is_a_credential_fault_not_a_request_fault() {
    let body = json!({"error": {
        "code": 400,
        "message": "Gemini API free tier is not available in your country. Please enable billing on your project in Google AI Studio.",
        "status": "FAILED_PRECONDITION"
    }})
    .to_string();
    let e = classify(
        ProviderKind::Gemini,
        Protocol::Gemini,
        400,
        &headers(&[JSON]),
        body.as_bytes(),
    );
    assert_ne!(
        e.class,
        FailureClass::Request,
        "a FAILED_PRECONDITION 400 must fail over to another credential"
    );
    assert!(e.class.should_failover());
    // Same envelope through the SSE-style one-element array.
    let wrapped = format!("[{body}]");
    let e = classify(
        ProviderKind::Gemini,
        Protocol::Gemini,
        400,
        &headers(&[JSON]),
        wrapped.as_bytes(),
    );
    assert_ne!(e.class, FailureClass::Request);
}

/// Track requirement: "401, 403 *with auth-type bodies* -> Auth".
///
/// Google answers a request that references a resource the calling project
/// cannot see — an uploaded File, a CachedContent — with 403
/// `PERMISSION_DENIED`. Nothing is wrong with the credential: the *request*
/// names something that belongs to another project. The classifier treats
/// every 403 that does not mention a model as `Auth`, and the scheduler
/// rests an `Auth` credential as a whole (`cooldown.auth_secs`, 30 min by
/// default) and fails over — so one request that references a file created
/// under key A takes every other Gemini credential it is tried on out of
/// rotation, for all models.
#[test]
fn a_google_403_about_a_referenced_resource_does_not_rest_the_credential() {
    let bodies = [
        "You do not have permission to access the File 8w1fdkj3tmbb or it may not exist.",
        "CachedContent not found (or permission denied)",
    ];
    for message in bodies {
        let body =
            json!({"error": {"code": 403, "message": message, "status": "PERMISSION_DENIED"}})
                .to_string();
        let e = classify(
            ProviderKind::Gemini,
            Protocol::Gemini,
            403,
            &headers(&[JSON]),
            body.as_bytes(),
        );
        assert!(
            !matches!(e.class, FailureClass::Auth | FailureClass::Quota),
            "`{message}` was classified {:?}, which cools the whole credential",
            e.class
        );
    }
    // A 403 that really is about the credential stays an auth failure.
    let body = json!({"error": {
        "code": 403,
        "message": "Your API key was reported as leaked. Please use another API key.",
        "status": "PERMISSION_DENIED"
    }})
    .to_string();
    let e = classify(
        ProviderKind::Gemini,
        Protocol::Gemini,
        403,
        &headers(&[JSON]),
        body.as_bytes(),
    );
    assert_eq!(e.class, FailureClass::Auth);
}

/// Notes 02 §10.3 / 11 §7.4: by status alone a failure is a request fault
/// "only for status 400, 409, 413, 422". Every other status error fails
/// over (and gets the transient cooldown). The classifier instead maps all
/// remaining 4xx to `Request`, so e.g. a forward proxy's 407, a CDN's 451
/// or a 405 from a mis-routed endpoint is returned to the client without
/// trying the next credential/provider.
#[test]
fn only_400_409_413_422_are_request_faults_by_status_alone() {
    let cases: [(u16, &str, &str); 4] = [
        (405, "text/plain", "Method Not Allowed"),
        (
            407,
            "text/html",
            "<html><head><title>407 Proxy Authentication Required</title></head><body>squid</body></html>",
        ),
        (421, "text/plain", "Misdirected Request"),
        (
            451,
            "text/html",
            "<html><head><title>Unavailable For Legal Reasons</title></head></html>",
        ),
    ];
    for (status, content_type, body) in cases {
        let e = classify(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            status,
            &headers(&[("content-type", content_type)]),
            body.as_bytes(),
        );
        assert!(
            e.class.should_failover(),
            "HTTP {status} was classified {:?}; another credential or provider must still be tried",
            e.class
        );
    }
    // The four statuses the notes do name stay request faults.
    for status in [400u16, 409, 413, 422] {
        let e = classify(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            status,
            &headers(&[("content-type", "text/plain")]),
            b"nope",
        );
        assert_eq!(e.class, FailureClass::Request, "HTTP {status}");
    }
}

/// `send()` reads up to 1 MiB of an error body and hands it to `classify`,
/// which is documented to accept a body that "may be arbitrarily large; at
/// most 64 KiB of it are kept". The envelope is however *parsed* from the
/// truncated copy: a JSON error longer than 64 KiB is cut mid-document, no
/// longer parses, and is treated as opaque text — type, code, message and
/// the body-aware classification are all lost.
#[test]
fn error_envelopes_longer_than_64_kib_are_still_understood() {
    // Compatible gateways echo large payloads / tracebacks inside errors.
    let padding = "x".repeat(70 * 1024);

    let quota = json!({"error": {
        "debug": padding,
        "message": "You exceeded your current quota, please check your plan and billing details.",
        "type": "insufficient_quota",
        "param": null,
        "code": "insufficient_quota"
    }})
    .to_string();
    let e = classify(
        ProviderKind::OpenaiCompat,
        Protocol::OpenaiChat,
        429,
        &headers(&[JSON]),
        quota.as_bytes(),
    );
    assert_eq!(e.class, FailureClass::Quota);
    assert_eq!(e.info.error_type.as_deref(), Some("insufficient_quota"));
    assert_eq!(
        e.info.message,
        "You exceeded your current quota, please check your plan and billing details."
    );
    // The kept copy is still bounded.
    assert!(e.body.as_deref().unwrap_or("").len() <= 64 * 1024);

    let missing = json!({"error": {
        "debug": padding,
        "message": "The model `gpt-x` does not exist or you do not have access to it.",
        "type": "invalid_request_error",
        "param": null,
        "code": "model_not_found"
    }})
    .to_string();
    let e = classify(
        ProviderKind::OpenaiCompat,
        Protocol::OpenaiChat,
        400,
        &headers(&[JSON]),
        missing.as_bytes(),
    );
    assert_eq!(e.class, FailureClass::ModelNotFound);
    assert_eq!(e.info.code.as_deref(), Some("model_not_found"));
}
