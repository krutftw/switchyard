//! Review finding GW-9: what an upstream said when it rejected the
//! *gateway's* credential is not shown to clients — not on the request that
//! ran into the rejection (covered by `resilience.rs`), and not on the
//! requests that arrive while the credential rests because of it.
//!
//! DESIGN.md section 8, step 6: "Upstream auth failures are reported as 502
//! (the client's key is fine)"; the track's requirement: "upstream 401/403
//! credential failures are never forwarded as-is (reply 502 with a generic
//! 'upstream rejected the gateway's credential' message — the client's own
//! key is fine)".
//!
//! A rejected credential rests for `routing.cooldown.auth_secs` (30 minutes
//! by default). Every request for the model during that time is answered
//! from the scheduler's `PickError::CoolingDown`, whose rendering quotes the
//! failure that started the rest: "(last upstream error: 401 Incorrect API
//! key provided: sk-proj-…)". So for half an hour every client is told, with
//! the upstream's own words, that "the API key" is incorrect — the very
//! confusion the 502 rule exists to prevent — and is shown the part of the
//! gateway's upstream key that the vendor echoes in its message (OpenAI
//! prints the first and last characters; the transport's scrubber only
//! recognises the complete key).

// The shared support module re-exports more than one review file uses.
#[allow(unused_imports)]
mod support;

use support::{Behaviour, Harness, Output};
use switchyard_core::Protocol;

const TWO_KEYS: &str = r#"
[routing]
strategy = "fill-first"

[[providers]]
name = "chat"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["sk-proj-gatewayAAAAAAAAAAAAAAAAAAAAAAAAwxyz", "sk-proj-gatewayBBBBBBBBBBBBBBBBBBBBBBBBstuv"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#;

/// What OpenAI answers a revoked key with: the key, masked by the vendor
/// itself (so it is not the string the gateway presented).
const REJECTION: &str = "Incorrect API key provided: sk-proj-********************wxyz. \
                         You can find your API key at https://platform.openai.com/account/api-keys.";

fn assert_private(what: &str, output: &Output) {
    let text = if output.streamed {
        output.wire_text()
    } else {
        String::from_utf8_lossy(&output.body).to_string()
    };
    assert!(
        !text.contains("Incorrect API key"),
        "{what}: the upstream's rejection of the gateway's credential is quoted to the client: {text}"
    );
    assert!(
        !text.contains("wxyz") && !text.contains("stuv"),
        "{what}: part of the gateway's upstream key is shown to the client: {text}"
    );
    assert!(
        !text.contains("platform.openai.com/account/api-keys"),
        "{what}: {text}"
    );
}

#[tokio::test]
async fn a_rejected_gateway_credential_stays_private_while_it_rests() {
    let harness = Harness::start(TWO_KEYS).await;
    for key in [
        "sk-proj-gatewayAAAAAAAAAAAAAAAAAAAAAAAAwxyz",
        "sk-proj-gatewayBBBBBBBBBBBBBBBBBBBBBBBBstuv",
    ] {
        harness.fake.always(key, Behaviour::error(401, REJECTION));
    }

    // The request that runs into the rejection: a generic 502.
    let first = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(first.status, 502);
    assert_private("the request that was rejected", &first);
    assert_eq!(harness.fake.count(), 2, "both credentials were tried");

    // Both credentials now rest (for `auth_secs`). Whoever asks meanwhile —
    // in whatever protocol, streaming or not — is not served, and is not
    // told what the upstream said about the gateway's key either.
    for (client, stream) in [
        (Protocol::OpenaiChat, false),
        (Protocol::OpenaiChat, true),
        (Protocol::Anthropic, false),
        (Protocol::Gemini, false),
        (Protocol::OpenaiResponses, true),
    ] {
        let later = harness.ask(client, "m", stream).await;
        assert!(
            later.status >= 400,
            "{client}: nothing can serve the model: {}",
            later.status
        );
        assert_private(
            &format!("{client} (stream={stream}) while the credentials rest"),
            &later,
        );
    }
    assert_eq!(
        harness.fake.count(),
        2,
        "resting credentials are not called"
    );
}
