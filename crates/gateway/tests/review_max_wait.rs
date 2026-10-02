//! Regression test (review finding GW-5): `routing.max_wait_secs` also helps
//! the request that ran into the cooldown.
//!
//! DESIGN.md section 8, step 5: "when every candidate is cooling down and
//! the soonest recovery is within `routing.max_wait_secs`, wait and retry;
//! otherwise fail with 429/503 and `Retry-After`."
//!
//! After a request's own attempt failed, its only credential is both tried
//! and resting. The request waits for it (once) and tries again instead of
//! failing at once, so the setting works in a single-credential deployment
//! too — not only for requests that arrive during the cooldown.

// The shared support module re-exports more than one review file uses.
#[allow(unused_imports)]
mod support;

use std::time::{Duration, Instant};
use support::{Behaviour, Harness};
use switchyard_core::Protocol;

#[tokio::test]
async fn a_rate_limited_request_waits_for_its_only_credential_within_max_wait() {
    let config = r#"
[routing]
max_wait_secs = 3
max_attempts = 3

[[providers]]
name = "chat"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-only"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#;
    let harness = Harness::start(config).await;
    // The upstream asks for one second, then answers normally.
    harness
        .fake
        .script("key-only", [Behaviour::rate_limited(1)]);

    let started = Instant::now();
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    let waited = started.elapsed();

    assert_eq!(
        output.status,
        200,
        "every candidate is cooling down for 1 s and max_wait_secs is 3: the request must wait and retry, not fail after {waited:?}: {}",
        String::from_utf8_lossy(&output.body)
    );
    assert!(waited >= Duration::from_millis(800), "waited {waited:?}");
    assert!(waited < Duration::from_secs(3), "waited {waited:?}");
    assert_eq!(harness.fake.count(), 2, "one failed call, one retry");
    let record = harness.record(&output.request_id);
    assert!(record.ok);
    assert_eq!(record.attempts.len(), 2);
}
