//! Regression tests (review finding SCHED-6): a request that pins a dated
//! snapshot is never served by a different model.
//!
//! DESIGN section 6, Registry: "Lookup is exact, then case-insensitive."
//! Version-suffix stripping is specified for the *catalog* (metadata
//! lookup), not for routing. Vendors use `-YYYY-MM-DD`, `-YYYYMMDD` and
//! `-NNN` suffixes to name distinct, separately priced snapshots
//! (`gpt-4o-2024-05-13` vs `gpt-4o` = a newer snapshot;
//! `gemini-2.0-flash-001`). A client that pins a snapshot the operator did
//! not expose must get `model_not_found`: were it routed to the undated
//! model, nobody could tell, because the gateway rewrites the response's
//! model name back to what the client asked for.
//!
//! (The opposite direction — an undated or `-latest` request finding the
//! registered snapshot — mirrors what the vendors' own aliases do and is
//! kept; see `registry.rs` in the tests directory.)

mod common;

use common::fixture;
use switchyard_scheduler::PickError;

#[test]
fn request_for_an_unregistered_dated_snapshot_is_not_rerouted_to_the_undated_model() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai-compat"
base_url = "http://upstream.test/v1"
api_keys = ["sk-key-a-0000000000000"]

[[providers.models]]
id = "gpt-4o"
"#,
    );
    // The operator exposes exactly one model.
    let listed: Vec<String> = f
        .scheduler
        .visible_models()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(listed, vec!["gpt-4o".to_string()]);

    for pinned in ["gpt-4o-2024-05-13", "gpt-4o-20240513", "gpt-4o-001"] {
        match f.scheduler.resolve(pinned) {
            Err(PickError::UnknownModel { model }) => assert_eq!(model, pinned),
            Ok(resolved) => panic!(
                "`{pinned}` is not a registered name but was routed to upstream model `{}`",
                resolved.targets[0].routes[0].upstream_model
            ),
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
}
