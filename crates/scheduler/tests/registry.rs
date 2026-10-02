//! Registry: credentials from config, model sets, client-facing names,
//! aliases and resolution.

mod common;

use common::{fixture, resolver};
use pretty_assertions::assert_eq;
use switchyard_core::config::ProviderKind;
use switchyard_core::util::mask_secret;
use switchyard_core::{Depth, Effort, MaxTokensField, ModelInfo, Protocol};
use switchyard_scheduler::{CredentialStatus, PickError, Scheduler, catalog};

fn visible_ids(s: &Scheduler) -> Vec<String> {
    s.visible_models().into_iter().map(|m| m.id).collect()
}

const OPENAI: &str = r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-openai-aaaaaaaaaaaaaaaaaaaa"]
"#;

// ---------------------------------------------------------------------------
// Model sets
// ---------------------------------------------------------------------------

#[test]
fn provider_without_models_serves_the_catalog_defaults_for_its_kind() {
    let f = fixture(OPENAI);
    let expected: Vec<String> = {
        let mut ids: Vec<String> = catalog()
            .models_for_kind(ProviderKind::Openai)
            .into_iter()
            .map(|m| m.id.clone())
            .collect();
        ids.sort();
        ids
    };
    assert!(expected.contains(&"gpt-5.5".to_string()));
    assert_eq!(visible_ids(&f.scheduler), expected);

    let resolved = f.scheduler.resolve("gpt-5.5").unwrap();
    assert_eq!(resolved.base, "gpt-5.5");
    assert_eq!(resolved.targets.len(), 1);
    let route = &resolved.targets[0].routes[0];
    assert_eq!(route.provider, "openai");
    assert_eq!(route.kind, ProviderKind::Openai);
    assert_eq!(route.upstream_model, "gpt-5.5");
    assert!(route.info.known);
    assert_eq!(route.info.context_window, Some(272_000));
}

#[test]
fn google_kinds_get_their_own_default_lists() {
    let f = fixture(
        r#"
[[providers]]
name = "gem"
kind = "gemini"
api_keys = ["AIza-gemini-key-000000000000"]
prefix = "g"

[[providers]]
name = "vx"
kind = "vertex"
api_keys = ["AIza-vertex-key-000000000000"]
prefix = "v"

[routing]
force_model_prefix = true
"#,
    );
    let ids = visible_ids(&f.scheduler);
    assert!(ids.contains(&"g/gemini-2.5-pro".to_string()));
    assert!(ids.contains(&"v/gemini-2.5-pro".to_string()));
    assert!(ids.contains(&"g/gemini-3-pro-preview".to_string()));
    assert!(!ids.contains(&"v/gemini-3-pro-preview".to_string()));
    assert!(ids.contains(&"v/gemini-3-pro".to_string()));
    assert!(
        !ids.iter()
            .any(|id| id.contains("claude") || id.contains("gpt"))
    );
}

#[test]
fn configured_models_replace_the_catalog_list() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-openai-aaaaaaaaaaaaaaaaaaaa"]

[[providers.models]]
id = "gpt-5.5"
alias = "smart"
display_name = "Smart"
max_output_tokens = 1000

[[providers.models]]
id = "gpt-6-sol"
"#,
    );
    assert_eq!(visible_ids(&f.scheduler), vec!["gpt-6-sol", "smart"]);
    // The upstream id of an aliased model is not a client-facing name.
    assert_eq!(
        f.scheduler.resolve("gpt-5.5").unwrap_err(),
        PickError::UnknownModel {
            model: "gpt-5.5".into()
        }
    );

    let lease = f.pick("smart").unwrap();
    assert_eq!(lease.upstream_model, "gpt-5.5");
    assert_eq!(lease.client_model, "smart");
    // Catalog entry for the upstream id, overlaid with the model config.
    assert_eq!(lease.info.id, "smart");
    assert_eq!(lease.info.display_name.as_deref(), Some("Smart"));
    assert_eq!(lease.info.max_output_tokens, Some(1000));
    assert_eq!(lease.info.context_window, Some(272_000));
    assert_eq!(lease.info.owned_by.as_deref(), Some("openai"));
    assert!(lease.info.known);
    assert_eq!(
        lease.info.thinking.as_ref().unwrap().levels,
        vec![Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh]
    );
}

#[test]
fn models_missing_from_the_catalog_are_bare_and_owned_by_the_provider() {
    let f = fixture(
        r#"
[[providers]]
name = "local"
kind = "openai-compat"
base_url = "http://localhost:11434/v1"

[[providers.models]]
id = "llama3.3"

[[providers.models]]
id = "qwen-think"
thinking = { levels = ["low", "high"] }
"#,
    );
    let lease = f.pick("llama3.3").unwrap();
    assert!(!lease.info.known);
    assert!(lease.info.thinking.is_none());
    assert_eq!(lease.info.owned_by.as_deref(), Some("local"));
    // A configured thinking block makes the model known.
    let lease = f.pick("qwen-think").unwrap();
    assert!(lease.info.known);
    assert_eq!(
        lease.info.thinking.unwrap().levels,
        vec![Effort::Low, Effort::High]
    );
}

#[test]
fn openai_compat_without_models_serves_nothing_until_discovery() {
    let f = fixture(
        r#"
[[providers]]
name = "local"
kind = "openai-compat"
base_url = "http://localhost:11434/v1"
"#,
    );
    assert!(visible_ids(&f.scheduler).is_empty());
    assert!(matches!(
        f.scheduler.resolve("llama3.3"),
        Err(PickError::UnknownModel { .. })
    ));

    assert!(f.scheduler.set_discovered(
        "local",
        vec![ModelInfo::bare("llama3.3"), ModelInfo::bare("phi4")]
    ));
    assert_eq!(visible_ids(&f.scheduler), vec!["llama3.3", "phi4"]);
    assert_eq!(f.pick("phi4").unwrap().upstream_model, "phi4");

    // An empty list forgets the discovered one.
    assert!(f.scheduler.set_discovered("local", vec![]));
    assert!(visible_ids(&f.scheduler).is_empty());
    assert!(
        !f.scheduler
            .set_discovered("nope", vec![ModelInfo::bare("x")])
    );
}

#[test]
fn discovered_list_replaces_catalog_defaults_and_overlays_metadata() {
    let f = fixture(
        r#"
[[providers]]
name = "claude"
kind = "anthropic"
api_keys = ["sk-ant-aaaaaaaaaaaaaaaaaaaa"]
"#,
    );
    assert!(visible_ids(&f.scheduler).contains(&"claude-opus-4-6".to_string()));

    f.scheduler.set_discovered(
        "claude",
        vec![
            ModelInfo {
                id: "claude-sonnet-4-5-20250929".into(),
                display_name: Some("Claude Sonnet 4.5 (upstream name)".into()),
                created: Some(1),
                ..ModelInfo::default()
            },
            ModelInfo::bare("claude-brand-new"),
        ],
    );
    assert_eq!(
        visible_ids(&f.scheduler),
        vec!["claude-brand-new", "claude-sonnet-4-5-20250929"]
    );
    let lease = f.pick("claude-sonnet-4-5-20250929").unwrap();
    // Upstream-stated fields win, the rest stays from the catalog.
    assert_eq!(
        lease.info.display_name.as_deref(),
        Some("Claude Sonnet 4.5 (upstream name)")
    );
    assert_eq!(lease.info.created, Some(1));
    assert_eq!(lease.info.max_output_tokens, Some(64_000));
    assert!(lease.info.known);
    assert!(lease.info.thinking.is_some());
    // A model the catalog does not know stays unknown.
    assert!(!f.pick("claude-brand-new").unwrap().info.known);
}

#[test]
fn configured_models_win_over_discovered_ones() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-openai-aaaaaaaaaaaaaaaaaaaa"]

[[providers.models]]
id = "gpt-5.5"
"#,
    );
    f.scheduler.set_discovered(
        "openai",
        vec![
            ModelInfo {
                id: "gpt-5.5".into(),
                context_window: Some(111),
                ..ModelInfo::default()
            },
            ModelInfo::bare("gpt-other"),
        ],
    );
    assert_eq!(visible_ids(&f.scheduler), vec!["gpt-5.5"]);
    // Discovered metadata still overlays the catalog entry.
    assert_eq!(f.pick("gpt-5.5").unwrap().info.context_window, Some(111));
}

#[test]
fn gemini_resource_names_are_normalised() {
    let f = fixture(
        r#"
[[providers]]
name = "gem"
kind = "gemini"
api_keys = ["AIza-gemini-key-000000000000"]
"#,
    );
    f.scheduler.set_discovered(
        "gem",
        vec![
            ModelInfo::bare("models/gemini-2.5-pro"),
            ModelInfo::bare("publishers/google/models/gemini-2.5-flash"),
        ],
    );
    assert_eq!(
        visible_ids(&f.scheduler),
        vec!["gemini-2.5-flash", "gemini-2.5-pro"]
    );
    let lease = f.pick("gemini-2.5-pro").unwrap();
    assert_eq!(lease.upstream_model, "gemini-2.5-pro");
    assert!(lease.info.known);
}

#[test]
fn mock_models_come_from_the_caller() {
    let f = fixture(
        r#"
[[providers]]
name = "demo"
kind = "mock"
"#,
    );
    assert!(visible_ids(&f.scheduler).is_empty());
    f.scheduler.set_discovered(
        "demo",
        vec![ModelInfo::bare("mock-echo"), ModelInfo::bare("mock-think")],
    );
    assert_eq!(visible_ids(&f.scheduler), vec!["mock-echo", "mock-think"]);
    let lease = f.pick("mock-echo").unwrap();
    assert_eq!(lease.credential.kind, ProviderKind::Mock);
    assert_eq!(lease.credential.base_url, "mock://local");
    assert_eq!(lease.credential.api_key, "");
    assert_eq!(lease.protocols, Protocol::ALL.to_vec());
}

#[test]
fn exclude_patterns_hide_models() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-openai-aaaaaaaaaaaaaaaaaaaa"]
exclude = ["gpt-6*", "*-LUNA"]
"#,
    );
    let ids = visible_ids(&f.scheduler);
    assert!(ids.contains(&"gpt-5.5".to_string()));
    assert!(ids.contains(&"gpt-5.6-sol".to_string()));
    assert!(!ids.iter().any(|id| id.starts_with("gpt-6")));
    assert!(!ids.iter().any(|id| id.ends_with("-luna")));
    assert!(matches!(
        f.scheduler.resolve("gpt-6-sol"),
        Err(PickError::UnknownModel { .. })
    ));
}

#[test]
fn exclude_matches_alias_and_upstream_id() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-openai-aaaaaaaaaaaaaaaaaaaa"]
exclude = ["fast", "gpt-6-sol"]

[[providers.models]]
id = "gpt-5.6-luna"
alias = "fast"

[[providers.models]]
id = "gpt-6-sol"
alias = "big"

[[providers.models]]
id = "gpt-5.5"
"#,
    );
    assert_eq!(visible_ids(&f.scheduler), vec!["gpt-5.5"]);
}

#[test]
fn disabled_providers_are_not_routable() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai"
enabled = false
api_keys = ["sk-openai-aaaaaaaaaaaaaaaaaaaa"]
"#,
    );
    assert!(visible_ids(&f.scheduler).is_empty());
    assert!(f.scheduler.resolve("gpt-5.5").is_err());
    let snapshot = f.scheduler.snapshot();
    assert_eq!(snapshot.len(), 1);
    assert!(!snapshot[0].enabled);
    assert_eq!(snapshot[0].credentials.len(), 1);
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

const PREFIXED: &str = r#"
[[providers]]
name = "team"
kind = "openai"
prefix = "/team-a/"
api_keys = ["sk-team-aaaaaaaaaaaaaaaaaaaaaa"]

[[providers.models]]
id = "gpt-5.5"

[[providers]]
name = "plain"
kind = "openai"
api_keys = ["sk-plain-aaaaaaaaaaaaaaaaaaaaa"]

[[providers.models]]
id = "gpt-5.5"
"#;

#[test]
fn prefixed_provider_serves_both_spellings() {
    let f = fixture(PREFIXED);
    assert_eq!(visible_ids(&f.scheduler), vec!["gpt-5.5", "team-a/gpt-5.5"]);

    let prefixed = f.scheduler.resolve("team-a/gpt-5.5").unwrap();
    assert_eq!(prefixed.base, "team-a/gpt-5.5");
    assert_eq!(prefixed.targets[0].routes.len(), 1);
    assert_eq!(prefixed.targets[0].routes[0].provider, "team");

    let bare = f.scheduler.resolve("gpt-5.5").unwrap();
    let providers: Vec<&str> = bare.targets[0]
        .routes
        .iter()
        .map(|r| r.provider.as_str())
        .collect();
    assert_eq!(providers, vec!["team", "plain"]);

    let lease = f.pick("team-a/gpt-5.5").unwrap();
    assert_eq!(lease.client_model, "team-a/gpt-5.5");
    assert_eq!(lease.info.id, "team-a/gpt-5.5");
    assert_eq!(lease.upstream_model, "gpt-5.5");
    assert_eq!(lease.credential.provider, "team");
}

#[test]
fn force_model_prefix_removes_the_bare_name_of_prefixed_providers() {
    let f = fixture(&format!(
        "{PREFIXED}\n[routing]\nforce_model_prefix = true\n"
    ));
    assert_eq!(visible_ids(&f.scheduler), vec!["gpt-5.5", "team-a/gpt-5.5"]);
    let bare = f.scheduler.resolve("gpt-5.5").unwrap();
    let providers: Vec<&str> = bare.targets[0]
        .routes
        .iter()
        .map(|r| r.provider.as_str())
        .collect();
    // Only the provider without a prefix serves the bare name now.
    assert_eq!(providers, vec!["plain"]);
    assert_eq!(
        f.pick("team-a/gpt-5.5").unwrap().credential.provider,
        "team"
    );
}

#[test]
fn force_model_prefix_with_only_prefixed_providers() {
    let f = fixture(
        r#"
[routing]
force_model_prefix = true

[[providers]]
name = "team"
kind = "openai"
prefix = "t"
api_keys = ["sk-team-aaaaaaaaaaaaaaaaaaaaaa"]

[[providers.models]]
id = "gpt-5.5"
"#,
    );
    assert_eq!(visible_ids(&f.scheduler), vec!["t/gpt-5.5"]);
    assert!(f.scheduler.resolve("gpt-5.5").is_err());
}

#[test]
fn lookup_is_exact_then_case_insensitive() {
    let f = fixture(
        r#"
[[providers]]
name = "a"
kind = "openai-compat"
base_url = "http://a.test/v1"

[[providers.models]]
id = "Model-X"

[[providers]]
name = "b"
kind = "openai-compat"
base_url = "http://b.test/v1"

[[providers.models]]
id = "model-x"
"#,
    );
    // Exact spelling picks the matching registration …
    assert_eq!(f.scheduler.resolve("Model-X").unwrap().base, "Model-X");
    assert_eq!(f.scheduler.resolve("model-x").unwrap().base, "model-x");
    // … any other casing falls back to a case-insensitive match (first
    // registered wins).
    let other = f.scheduler.resolve("MODEL-X").unwrap();
    assert_eq!(other.base, "Model-X");
    assert_eq!(other.requested, "MODEL-X");
    assert_eq!(
        f.scheduler.resolve("  model-x  ").unwrap().requested,
        "model-x"
    );
}

#[test]
fn floating_names_find_snapshots_but_pinned_snapshots_are_never_substituted() {
    let f = fixture(
        r#"
[[providers]]
name = "claude"
kind = "anthropic"
api_keys = ["sk-ant-aaaaaaaaaaaaaaaaaaaa"]

[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-openai-aaaaaaaaaaaaaaaaaaaa"]
"#,
    );
    // Undated alias → the dated snapshot that is registered.
    let r = f.scheduler.resolve("claude-sonnet-4-5").unwrap();
    assert_eq!(r.base, "claude-sonnet-4-5-20250929");
    assert_eq!(
        r.targets[0].routes[0].upstream_model,
        "claude-sonnet-4-5-20250929"
    );
    // `-latest` floats too: the name without it, or its newest snapshot.
    let r = f.scheduler.resolve("claude-opus-4-6-latest(high)").unwrap();
    assert_eq!(r.base, "claude-opus-4-6");
    assert_eq!(r.suffix_depth, Some(Depth::Level(Effort::High)));
    assert_eq!(
        f.scheduler
            .resolve("Claude-Sonnet-4-5-LATEST")
            .unwrap()
            .base,
        "claude-sonnet-4-5-20250929"
    );
    // A pinned snapshot names one particular model. When it is not
    // registered it is unknown — never quietly served by the undated model.
    for pinned in [
        "gpt-5.5-2026-04-23",
        "gpt-5.5-20260423",
        "gpt-5.5-001",
        "claude-opus-4-6-20260101",
        "claude-sonnet-4-5-20240101",
    ] {
        assert_eq!(
            f.scheduler.resolve(pinned).unwrap_err(),
            PickError::UnknownModel {
                model: pinned.into()
            },
        );
    }
    assert!(f.scheduler.resolve("gpt-5.5-turbo").is_err());
    assert!(f.scheduler.resolve("-latest").is_err());
    assert!(f.scheduler.resolve("nothing-latest").is_err());
}

#[test]
fn alias_targets_follow_the_same_snapshot_rules() {
    let f = fixture(
        r#"
[[providers]]
name = "claude"
kind = "anthropic"
api_keys = ["sk-ant-aaaaaaaaaaaaaaaaaaaa"]

[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-openai-aaaaaaaaaaaaaaaaaaaa"]

[[aliases]]
name = "smart"
targets = ["gpt-5.5-2026-04-23", "claude-sonnet-4-5"]
"#,
    );
    // The pinned target matches nothing and is reported; the floating one
    // resolves to the registered snapshot.
    let r = f.scheduler.resolve("smart").unwrap();
    let targets: Vec<&str> = r.targets.iter().map(|t| t.client_model.as_str()).collect();
    assert_eq!(targets, vec!["claude-sonnet-4-5-20250929"]);
    assert!(
        f.scheduler
            .warnings()
            .iter()
            .any(|w| w.contains("gpt-5.5-2026-04-23") && w.contains("matches no model")),
        "{:?}",
        f.scheduler.warnings()
    );
}

// ---------------------------------------------------------------------------
// Reasoning suffixes
// ---------------------------------------------------------------------------

#[test]
fn resolve_splits_reasoning_suffixes() {
    let f = fixture(OPENAI);
    let cases = [
        ("gpt-5.5", None),
        ("gpt-5.5(high)", Some(Depth::Level(Effort::High))),
        ("gpt-5.5(minimal)", Some(Depth::Level(Effort::Minimal))),
        ("gpt-5.5(8192)", Some(Depth::Budget(8192))),
        ("gpt-5.5(none)", Some(Depth::Off)),
        ("gpt-5.5(0)", Some(Depth::Off)),
        ("gpt-5.5(auto)", Some(Depth::Auto)),
        ("gpt-5.5(-1)", Some(Depth::Auto)),
        // Unrecognised suffix: removed from the name, no directive.
        ("gpt-5.5(ultra)", None),
        ("GPT-5.5(HIGH)", Some(Depth::Level(Effort::High))),
    ];
    for (input, depth) in cases {
        let r = f.scheduler.resolve(input).unwrap();
        assert_eq!(r.requested, input);
        assert_eq!(r.base, "gpt-5.5", "{input}");
        assert_eq!(r.suffix_depth, depth, "{input}");
        assert_eq!(r.targets.len(), 1);
        assert_eq!(r.targets[0].client_model, "gpt-5.5");
        assert_eq!(r.targets[0].pinned_depth, None);
    }
    assert_eq!(
        f.scheduler.resolve("nope(high)").unwrap_err(),
        PickError::UnknownModel {
            model: "nope(high)".into()
        }
    );
    assert!(f.scheduler.resolve("").is_err());
    assert!(f.scheduler.resolve("(high)").is_err());
}

#[test]
fn full_string_is_tried_as_a_literal_model_name() {
    let f = fixture(
        r#"
[[providers]]
name = "local"
kind = "openai-compat"
base_url = "http://localhost:1234/v1"

[[providers.models]]
id = "weird(model)"

[[providers.models]]
id = "qwen(high)"
"#,
    );
    let r = f.scheduler.resolve("weird(model)").unwrap();
    assert_eq!(r.base, "weird(model)");
    assert_eq!(r.suffix_depth, None);
    assert_eq!(r.targets[0].routes[0].upstream_model, "weird(model)");
    // Even when the parentheses look like a reasoning suffix: `qwen` is not
    // a model, so the literal id is used and no depth is implied.
    let r = f.scheduler.resolve("qwen(high)").unwrap();
    assert_eq!(r.base, "qwen(high)");
    assert_eq!(r.suffix_depth, None);
}

// ---------------------------------------------------------------------------
// Aliases
// ---------------------------------------------------------------------------

const TWO_VENDORS: &str = r#"
[[providers]]
name = "claude"
kind = "anthropic"
api_keys = ["sk-ant-aaaaaaaaaaaaaaaaaaaa"]

[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-openai-aaaaaaaaaaaaaaaaaaaa"]
"#;

#[test]
fn alias_resolves_to_ordered_targets_with_pinned_depths() {
    let f = fixture(&format!(
        "{TWO_VENDORS}\n[[aliases]]\nname = \"smart\"\ntargets = [\"claude-opus-4-6\", \"gpt-5.5(high)\"]\n"
    ));
    let r = f.scheduler.resolve("smart").unwrap();
    assert_eq!(r.base, "smart");
    assert_eq!(r.suffix_depth, None);
    assert_eq!(r.targets.len(), 2);
    assert_eq!(r.targets[0].client_model, "claude-opus-4-6");
    assert_eq!(r.targets[0].pinned_depth, None);
    assert_eq!(r.targets[0].routes[0].provider, "claude");
    assert_eq!(r.targets[1].client_model, "gpt-5.5");
    assert_eq!(r.targets[1].pinned_depth, Some(Depth::Level(Effort::High)));
    assert_eq!(r.targets[1].routes[0].provider, "openai");

    assert!(visible_ids(&f.scheduler).contains(&"smart".to_string()));
    assert!(visible_ids(&f.scheduler).contains(&"gpt-5.5".to_string()));
    // Alias names are matched case-insensitively too.
    assert_eq!(f.scheduler.resolve("SMART").unwrap().base, "smart");
    assert!(
        f.scheduler.warnings().is_empty(),
        "{:?}",
        f.scheduler.warnings()
    );
}

#[test]
fn pinned_depth_beats_the_client_suffix() {
    let f = fixture(&format!(
        "{TWO_VENDORS}\n[[aliases]]\nname = \"smart\"\ntargets = [\"claude-opus-4-6\", \"gpt-5.5(high)\"]\n"
    ));
    let resolved = f.scheduler.resolve("smart(low)").unwrap();
    assert_eq!(resolved.base, "smart");
    assert_eq!(resolved.suffix_depth, Some(Depth::Level(Effort::Low)));

    // First target has no pin: the client's suffix applies.
    let first = f.pick("smart(low)").unwrap();
    assert_eq!(first.client_model, "claude-opus-4-6");
    assert_eq!(first.pinned_depth, None);
    assert_eq!(resolved.depth_for(&first), Some(Depth::Level(Effort::Low)));

    // Second target pins `high`, which wins.
    let second = f
        .pick_with(
            "smart(low)",
            std::slice::from_ref(&first.credential.id),
            None,
        )
        .unwrap();
    assert_eq!(second.client_model, "gpt-5.5");
    assert_eq!(second.upstream_model, "gpt-5.5");
    assert_eq!(second.info.id, "gpt-5.5");
    assert_eq!(second.pinned_depth, Some(Depth::Level(Effort::High)));
    assert_eq!(
        resolved.depth_for(&second),
        Some(Depth::Level(Effort::High))
    );
}

#[test]
fn hide_targets_hides_from_listings_but_keeps_routable() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-openai-aaaaaaaaaaaaaaaaaaaa"]

[[providers.models]]
id = "gpt-5.5"

[[providers.models]]
id = "gpt-6-sol"

[[providers.models]]
id = "gpt-6-luna"

[[aliases]]
name = "best"
targets = ["gpt-6-sol", "gpt-5.5(high)"]
hide_targets = true
"#,
    );
    assert_eq!(visible_ids(&f.scheduler), vec!["best", "gpt-6-luna"]);
    // Still routable by their own names.
    assert_eq!(f.pick("gpt-5.5").unwrap().upstream_model, "gpt-5.5");
    assert_eq!(f.pick("gpt-6-sol").unwrap().upstream_model, "gpt-6-sol");

    let table = f.scheduler.models();
    let hidden: Vec<(&str, bool)> = table.iter().map(|m| (m.name.as_str(), m.hidden)).collect();
    assert_eq!(
        hidden,
        vec![
            ("best", false),
            ("gpt-5.5", true),
            ("gpt-6-luna", false),
            ("gpt-6-sol", true)
        ]
    );
}

#[test]
fn alias_of_alias_is_flattened_and_innermost_pin_wins() {
    let f = fixture(&format!(
        "{TWO_VENDORS}
[[aliases]]
name = \"outer\"
targets = [\"inner(low)\"]

[[aliases]]
name = \"inner\"
targets = [\"gpt-5.5\", \"claude-opus-4-6(high)\"]
"
    ));
    let r = f.scheduler.resolve("outer").unwrap();
    let targets: Vec<(&str, Option<Depth>)> = r
        .targets
        .iter()
        .map(|t| (t.client_model.as_str(), t.pinned_depth))
        .collect();
    assert_eq!(
        targets,
        vec![
            ("gpt-5.5", Some(Depth::Level(Effort::Low))),
            ("claude-opus-4-6", Some(Depth::Level(Effort::High))),
        ]
    );
    assert!(
        f.scheduler.warnings().is_empty(),
        "{:?}",
        f.scheduler.warnings()
    );
}

#[test]
fn alias_cycles_are_cut_and_reported() {
    let f = fixture(&format!(
        "{TWO_VENDORS}
[[aliases]]
name = \"a\"
targets = [\"b\"]

[[aliases]]
name = \"b\"
targets = [\"a\", \"gpt-5.5\"]
"
    ));
    let a = f.scheduler.resolve("a").unwrap();
    assert_eq!(a.targets.len(), 1);
    assert_eq!(a.targets[0].client_model, "gpt-5.5");
    let b = f.scheduler.resolve("b").unwrap();
    assert_eq!(b.targets.len(), 1);
    assert_eq!(b.targets[0].client_model, "gpt-5.5");
    let warnings = f.scheduler.warnings();
    assert!(warnings.iter().any(|w| w.contains("cycle")), "{warnings:?}");
}

#[test]
fn alias_named_like_a_model_can_pin_its_depth() {
    let f = fixture(&format!(
        "{OPENAI}\n[[aliases]]\nname = \"gpt-5.5\"\ntargets = [\"gpt-5.5(high)\"]\n"
    ));
    let r = f.scheduler.resolve("gpt-5.5").unwrap();
    assert_eq!(r.targets.len(), 1);
    assert_eq!(r.targets[0].client_model, "gpt-5.5");
    assert_eq!(r.targets[0].pinned_depth, Some(Depth::Level(Effort::High)));
    assert!(
        f.scheduler.warnings().is_empty(),
        "{:?}",
        f.scheduler.warnings()
    );
    // Listed once.
    let ids = visible_ids(&f.scheduler);
    assert_eq!(ids.iter().filter(|id| *id == "gpt-5.5").count(), 1);
    let lease = f.pick("gpt-5.5(low)").unwrap();
    assert_eq!(lease.pinned_depth, Some(Depth::Level(Effort::High)));
}

#[test]
fn unroutable_alias_targets_are_reported() {
    let f = fixture(&format!(
        "{OPENAI}
[[aliases]]
name = \"partly\"
targets = [\"does-not-exist\", \"gpt-5.5\"]

[[aliases]]
name = \"broken\"
targets = [\"also-missing\"]
"
    ));
    let partly = f.scheduler.resolve("partly").unwrap();
    assert_eq!(partly.targets.len(), 1);
    assert_eq!(partly.targets[0].client_model, "gpt-5.5");
    assert!(matches!(
        f.scheduler.resolve("broken"),
        Err(PickError::UnknownModel { .. })
    ));
    let warnings = f.scheduler.warnings();
    assert!(
        warnings.iter().any(|w| w.contains("does-not-exist")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w.contains("broken")),
        "{warnings:?}"
    );
    assert!(!visible_ids(&f.scheduler).contains(&"broken".to_string()));
}

#[test]
fn alias_targets_may_use_prefixed_names() {
    let f = fixture(&format!(
        "{PREFIXED}\n[[aliases]]\nname = \"mine\"\ntargets = [\"team-a/gpt-5.5(medium)\"]\n"
    ));
    let lease = f.pick("mine").unwrap();
    assert_eq!(lease.credential.provider, "team");
    assert_eq!(lease.client_model, "team-a/gpt-5.5");
    assert_eq!(lease.pinned_depth, Some(Depth::Level(Effort::Medium)));
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

#[test]
fn credential_ids_are_stable_hashes_that_never_contain_the_key() {
    let toml = r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-first-aaaaaaaaaaaaaaaaaaaa", "sk-second-bbbbbbbbbbbbbbbbbbb"]
"#;
    let a = fixture(toml);
    let b = fixture(toml);
    let ids: Vec<String> = a.all_credentials().into_iter().map(|c| c.id).collect();
    let again: Vec<String> = b.all_credentials().into_iter().map(|c| c.id).collect();
    assert_eq!(ids, again);
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    for id in &ids {
        let (provider, hash) = id.split_once(':').unwrap();
        assert_eq!(provider, "openai");
        assert_eq!(hash.len(), 12);
        assert!(hash.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        assert!(!id.contains("sk-"));
    }
}

#[test]
fn credential_id_depends_on_provider_kind_key_and_base_url() {
    let id_of = |toml: &str| fixture(toml).all_credentials()[0].id.clone();
    let base = id_of(
        "[[providers]]\nname = \"p\"\nkind = \"openai\"\napi_keys = [\"sk-key-aaaaaaaaaaaaaaaa\"]\n",
    );
    let other_key = id_of(
        "[[providers]]\nname = \"p\"\nkind = \"openai\"\napi_keys = [\"sk-key-bbbbbbbbbbbbbbbb\"]\n",
    );
    let other_url = id_of(
        "[[providers]]\nname = \"p\"\nkind = \"openai\"\nbase_url = \"https://eu.example/v1\"\napi_keys = [\"sk-key-aaaaaaaaaaaaaaaa\"]\n",
    );
    let other_kind = id_of(
        "[[providers]]\nname = \"p\"\nkind = \"anthropic\"\napi_keys = [\"sk-key-aaaaaaaaaaaaaaaa\"]\n",
    );
    let other_name = id_of(
        "[[providers]]\nname = \"q\"\nkind = \"openai\"\napi_keys = [\"sk-key-aaaaaaaaaaaaaaaa\"]\n",
    );
    // Settings that are not part of the identity.
    let same = id_of(
        "[[providers]]\nname = \"p\"\nkind = \"openai\"\npriority = 5\nprefix = \"x\"\nproxy = \"direct\"\n\n[[providers.credentials]]\napi_key = \"sk-key-aaaaaaaaaaaaaaaa\"\nlabel = \"main\"\nweight = 7\n",
    );
    assert_eq!(base, same);
    for different in [&other_key, &other_url, &other_kind, &other_name] {
        assert_ne!(&base, different);
    }
    assert!(other_name.starts_with("q:"));
}

#[test]
fn duplicate_credentials_get_numbered_ids() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-dup-aaaaaaaaaaaaaaaaaaaa", "sk-dup-aaaaaaaaaaaaaaaaaaaa", "sk-dup-aaaaaaaaaaaaaaaaaaaa"]
"#,
    );
    let ids: Vec<String> = f.all_credentials().into_iter().map(|c| c.id).collect();
    assert_eq!(ids[1], format!("{}-1", ids[0]));
    assert_eq!(ids[2], format!("{}-2", ids[0]));
}

#[test]
fn labels_default_to_the_masked_key_or_file_name() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-proj-abcdefghijklmnopqrstuvwxyz"]

[[providers.credentials]]
api_key = "sk-labelled-0000000000000000"
label = "team key"

[[providers]]
name = "vx"
kind = "vertex"
project = "my-project"
location = "europe-west4"

[[providers.credentials]]
service_account_file = "keys/vertex-sa.json"

[[providers]]
name = "local"
kind = "openai-compat"
base_url = "http://localhost:11434/v1/"
"#,
    );
    let all = f.all_credentials();
    assert_eq!(
        all[0].label,
        mask_secret("sk-proj-abcdefghijklmnopqrstuvwxyz")
    );
    assert_eq!(all[0].label, "sk-pro…wxyz");
    assert_eq!(all[0].masked_key, "sk-pro…wxyz");
    assert_eq!(all[1].label, "team key");
    assert_eq!(
        all[1].masked_key,
        mask_secret("sk-labelled-0000000000000000")
    );
    assert_eq!(all[2].label, "vertex-sa.json");
    assert_eq!(all[2].masked_key, "vertex-sa.json");
    // Keyless credential of a local server: labelled after its provider.
    assert_eq!(all[3].label, "local");
    assert_eq!(all[3].masked_key, "");
    assert!(
        all.iter()
            .all(|c| c.usable && c.status == CredentialStatus::Ready)
    );

    let vertex = f.scheduler.credential(&all[2].id).unwrap();
    assert_eq!(vertex.service_account_file, "keys/vertex-sa.json");
    assert_eq!(vertex.api_key, "");
    assert_eq!(vertex.project, "my-project");
    assert_eq!(vertex.location, "europe-west4");
    assert_eq!(vertex.base_url, "https://aiplatform.googleapis.com");

    let local = f.scheduler.credential(&all[3].id).unwrap();
    assert_eq!(local.base_url, "http://localhost:11434/v1");
    assert_eq!(local.api_key, "");
}

#[test]
fn unset_environment_secrets_make_a_credential_unusable() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["env:MISSING_OPENAI_KEY", "env:SET_OPENAI_KEY"]
"#,
    );
    let all = f.all_credentials();
    assert!(!all[0].usable);
    assert_eq!(
        all[0].unusable_reason.as_deref(),
        Some("environment variable MISSING_OPENAI_KEY is not set")
    );
    assert_eq!(all[0].status, CredentialStatus::Unusable);
    // References are shown as written.
    assert_eq!(all[0].masked_key, "env:MISSING_OPENAI_KEY");
    assert_eq!(all[0].label, "env:MISSING_OPENAI_KEY");
    assert!(all[1].usable);
    assert_eq!(all[1].unusable_reason, None);
    assert_eq!(all[1].masked_key, mask_secret("sk-from-env-SET_OPENAI_KEY"));

    // Never selected.
    for _ in 0..5 {
        assert_eq!(f.pick_key("gpt-5.5"), "sk-from-env-SET_OPENAI_KEY");
    }
    let warnings = f.scheduler.warnings();
    assert!(
        warnings.iter().any(|w| w.contains("MISSING_OPENAI_KEY")),
        "{warnings:?}"
    );
    assert_eq!(f.scheduler.credentials("openai").len(), 1);
}

#[test]
fn model_with_only_unusable_credentials_reports_no_credentials() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["${MISSING_KEY}"]

[[providers]]
name = "empty"
kind = "anthropic"
"#,
    );
    assert_eq!(
        f.pick("gpt-5.5").unwrap_err(),
        PickError::NoCredentials {
            model: "gpt-5.5".into()
        }
    );
    assert_eq!(
        f.all_credentials()[0].unusable_reason.as_deref(),
        Some("environment variable MISSING_KEY is not set")
    );
    // A provider with no credentials at all: model known, nothing to use.
    assert_eq!(
        f.pick("claude-opus-4-6").unwrap_err(),
        PickError::NoCredentials {
            model: "claude-opus-4-6".into()
        }
    );
    assert_eq!(f.scheduler.soonest_recovery("gpt-5.5"), None);
}

#[test]
fn credential_view_carries_transport_settings() {
    let f = fixture(
        r#"
[upstream]
proxy = "http://global-proxy.test:8080"

[[providers]]
name = "a"
kind = "openai"
api_keys = ["sk-a-plain-aaaaaaaaaaaaaaaa"]
headers = { "X-Org" = " acme ", "X-Empty" = "" }

[[providers.credentials]]
api_key = "sk-a-own-proxy-aaaaaaaaaaaa"
proxy = "socks5://127.0.0.1:1080"

[[providers]]
name = "b"
kind = "openai"
proxy = "direct"
api_keys = ["sk-b-aaaaaaaaaaaaaaaaaaaaaa"]
"#,
    );
    let a = f.scheduler.credentials("a");
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].proxy, "http://global-proxy.test:8080");
    assert_eq!(a[1].proxy, "socks5://127.0.0.1:1080");
    assert_eq!(a[0].base_url, "https://api.openai.com/v1");
    let headers: Vec<(&str, &str)> = a[0]
        .headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(headers, vec![("X-Org", "acme")]);
    let b = f.scheduler.credentials("b");
    assert_eq!(b[0].proxy, "direct");
    assert!(f.scheduler.credentials("nope").is_empty());
}

#[test]
fn debug_output_never_shows_the_key() {
    let f = fixture(OPENAI);
    let lease = f.pick("gpt-5.5").unwrap();
    let printed = format!("{lease:?} {:?} {:?}", lease.credential, f.scheduler);
    assert!(
        !printed.contains("sk-openai-aaaaaaaaaaaaaaaaaaaa"),
        "{printed}"
    );
    assert!(printed.contains("sk-ope…aaaa"));
}

// ---------------------------------------------------------------------------
// Lease details
// ---------------------------------------------------------------------------

#[test]
fn upstream_protocol_follows_the_client_when_supported() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-openai-aaaaaaaaaaaaaaaaaaaa"]

[[providers]]
name = "claude"
kind = "anthropic"
api_keys = ["sk-ant-aaaaaaaaaaaaaaaaaaaa"]

[[providers]]
name = "compat"
kind = "openai-compat"
base_url = "http://localhost:8000/v1"

[[providers.models]]
id = "local-model"

[[providers]]
name = "chatonly"
kind = "openai"
wire_api = "chat"
legacy_max_tokens = true
api_keys = ["sk-chatonly-aaaaaaaaaaaaaaaaaa"]

[[providers.models]]
id = "gpt-chat-only"
"#,
    );
    let pick = |model: &str, protocol| f.pick_as(model, &[], None, protocol).unwrap();

    let lease = pick("gpt-5.5", Protocol::OpenaiChat);
    assert_eq!(
        lease.protocols,
        vec![Protocol::OpenaiResponses, Protocol::OpenaiChat]
    );
    assert_eq!(lease.upstream_protocol, Protocol::OpenaiChat);
    assert_eq!(
        pick("gpt-5.5", Protocol::OpenaiResponses).upstream_protocol,
        Protocol::OpenaiResponses
    );
    // A client of another protocol gets the provider's first.
    assert_eq!(
        pick("gpt-5.5", Protocol::Anthropic).upstream_protocol,
        Protocol::OpenaiResponses
    );
    assert_eq!(
        pick("claude-opus-4-6", Protocol::Gemini).upstream_protocol,
        Protocol::Anthropic
    );
    assert_eq!(
        pick("local-model", Protocol::OpenaiResponses).upstream_protocol,
        Protocol::OpenaiChat
    );

    // Quirks: the output-limit field differs per provider flavour.
    assert_eq!(
        pick("gpt-5.5", Protocol::OpenaiChat)
            .quirks
            .max_tokens_field,
        MaxTokensField::MaxCompletionTokens
    );
    assert_eq!(
        pick("local-model", Protocol::OpenaiChat)
            .quirks
            .max_tokens_field,
        MaxTokensField::MaxTokens
    );
    let chat_only = pick("gpt-chat-only", Protocol::OpenaiResponses);
    assert_eq!(chat_only.upstream_protocol, Protocol::OpenaiChat);
    assert_eq!(chat_only.quirks.max_tokens_field, MaxTokensField::MaxTokens);
    assert_eq!(chat_only.provider_config.name, "chatonly");
    assert_eq!(chat_only.credential.kind, ProviderKind::Openai);
}

#[test]
fn stale_resolution_after_the_provider_vanished_is_an_unknown_model() {
    let f = fixture(OPENAI);
    let resolved = f.scheduler.resolve("gpt-5.5").unwrap();
    f.rebuild("");
    let err = f
        .scheduler
        .pick(&switchyard_scheduler::PickRequest {
            resolved: &resolved,
            tried: &[],
            session: None,
            client_protocol: Protocol::OpenaiChat,
            now: f.scheduler.now(),
        })
        .unwrap_err();
    assert_eq!(
        err,
        PickError::UnknownModel {
            model: "gpt-5.5".into()
        }
    );
}

#[test]
fn retain_routes_restricts_a_resolution_to_some_providers() {
    let f = fixture(
        r#"
[[providers]]
name = "compat"
kind = "openai-compat"
base_url = "http://localhost:8000/v1"

[[providers.models]]
id = "gpt-5.5"

[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-openai-aaaaaaaaaaaaaaaaaaaa"]

[[providers.models]]
id = "gpt-5.5"
"#,
    );
    let mut resolved = f.scheduler.resolve("gpt-5.5").unwrap();
    assert_eq!(resolved.targets[0].routes.len(), 2);
    resolved.retain_routes(|r| r.kind == ProviderKind::Openai);
    assert!(resolved.has_routes());
    for _ in 0..4 {
        let lease = f
            .scheduler
            .pick(&switchyard_scheduler::PickRequest {
                resolved: &resolved,
                tried: &[],
                session: None,
                client_protocol: Protocol::OpenaiResponses,
                now: f.scheduler.now(),
            })
            .unwrap();
        assert_eq!(lease.credential.provider, "openai");
    }
    resolved.retain_routes(|_| false);
    assert!(!resolved.has_routes());
    assert!(resolved.targets.is_empty());
}

#[test]
fn resolver_is_only_asked_for_configured_secrets() {
    use std::cell::RefCell;
    let seen = RefCell::new(Vec::new());
    let recording = |value: &str| {
        seen.borrow_mut().push(value.to_string());
        resolver(value)
    };
    let config = common::config(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = [" sk-literal-aaaaaaaaaaaaaaaa ", "env:SET_K"]

[[providers]]
name = "local"
kind = "openai-compat"
base_url = "http://localhost:1/v1"
"#,
    );
    let scheduler = Scheduler::new(&config, &recording);
    assert_eq!(
        *seen.borrow(),
        vec![
            "sk-literal-aaaaaaaaaaaaaaaa".to_string(),
            "env:SET_K".to_string()
        ]
    );
    assert_eq!(scheduler.snapshot()[0].credentials.len(), 2);
}
