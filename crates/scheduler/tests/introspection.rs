//! Snapshots, the model table, runtime control and rebuilding.

mod common;

use common::{config, error, fixture, resolver, secs};
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use switchyard_core::{FailureClass, ModelInfo, Protocol};
use switchyard_scheduler::{
    CredentialStatus, ModelRoute, Outcome, PickError, PickRequest, Scheduler,
};

const SECRETS: [&str; 4] = [
    "sk-proj-SECRETSECRETSECRET-one",
    "sk-proj-SECRETSECRETSECRET-two",
    "sk-ant-SECRETSECRETSECRET-three",
    "sk-from-env-SET_FOURTH_KEY",
];

const FULL: &str = r#"
[[providers]]
name = "openai"
kind = "openai"
priority = 2
api_keys = ["sk-proj-SECRETSECRETSECRET-one"]

[[providers.credentials]]
api_key = "sk-proj-SECRETSECRETSECRET-two"
label = "second"
weight = 4
priority = 7

[[providers.credentials]]
api_key = "env:SET_FOURTH_KEY"
label = "from env"
disabled = true

[[providers.credentials]]
api_key = "env:MISSING_FIFTH_KEY"

[[providers.models]]
id = "gpt-5.5"

[[providers.models]]
id = "gpt-6-sol"
alias = "sol"

[[providers]]
name = "claude"
kind = "anthropic"
prefix = "ant"
api_keys = ["sk-ant-SECRETSECRETSECRET-three"]

[[providers.models]]
id = "claude-opus-4-6"

[[aliases]]
name = "smart"
targets = ["claude-opus-4-6", "gpt-5.5(high)"]
"#;

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

#[test]
fn snapshot_lists_providers_and_credentials_in_config_order() {
    let f = fixture(FULL);
    let snapshot = f.scheduler.snapshot();
    assert_eq!(snapshot.len(), 2);
    assert_eq!(snapshot[0].name, "openai");
    assert_eq!(snapshot[0].models, 2);
    assert!(snapshot[0].enabled);
    assert_eq!(snapshot[1].name, "claude");
    assert_eq!(snapshot[1].models, 1);

    let creds = &snapshot[0].credentials;
    assert_eq!(creds.len(), 4);
    let statuses: Vec<CredentialStatus> = creds.iter().map(|c| c.status).collect();
    assert_eq!(
        statuses,
        vec![
            CredentialStatus::Ready,
            CredentialStatus::Ready,
            CredentialStatus::Disabled,
            CredentialStatus::Unusable,
        ]
    );
    assert_eq!(creds[0].weight, 1);
    assert_eq!(creds[0].priority, 2);
    assert_eq!(creds[1].label, "second");
    assert_eq!(creds[1].weight, 4);
    assert_eq!(creds[1].priority, 7);
    assert!(creds[2].disabled && creds[2].usable);
    assert!(!creds[3].disabled && !creds[3].usable);
}

#[test]
fn snapshot_serialises_in_the_documented_shape() {
    let f = fixture(FULL);
    let snapshot = f.scheduler.snapshot();
    let value = serde_json::to_value(&snapshot).unwrap();
    let id = snapshot[0].credentials[0].id.clone();

    assert_eq!(
        value[0]["credentials"][0],
        json!({
            "id": id,
            "label": "sk-pro…-one",
            "masked_key": "sk-pro…-one",
            "disabled": false,
            "usable": true,
            "status": "ready",
            "model_cooldowns": [],
            "requests": 0,
            "successes": 0,
            "failures": 0,
            "consecutive_failures": 0,
            "weight": 1,
            "priority": 2
        })
    );
    assert_eq!(value[0]["name"], "openai");
    assert_eq!(value[0]["kind"], "openai");
    assert_eq!(value[0]["enabled"], true);
    assert_eq!(value[0]["models"], 2);
    assert_eq!(value[0]["credentials"][2]["status"], "disabled");
    assert_eq!(value[0]["credentials"][3]["status"], "unusable");
    assert_eq!(
        value[0]["credentials"][3]["unusable_reason"],
        "environment variable MISSING_FIFTH_KEY is not set"
    );
    assert_eq!(
        value[0]["credentials"][3]["masked_key"],
        "env:MISSING_FIFTH_KEY"
    );
    assert_eq!(value[1]["kind"], "anthropic");
}

#[test]
fn snapshot_shows_cooldowns_counters_and_errors() {
    let f = fixture(FULL);
    let claude = f.pick("ant/claude-opus-4-6").unwrap();
    f.succeed(&claude, 420);
    f.advance(2);
    f.fail(&claude, &error(FailureClass::Auth, None));

    let openai = f.pick("gpt-5.5").unwrap();
    f.fail(&openai, &error(FailureClass::RateLimit, Some(9_000)));

    let now = f.now_ms();
    let value = serde_json::to_value(f.scheduler.snapshot()).unwrap();
    let c = &value[1]["credentials"][0];
    assert_eq!(c["status"], "cooling");
    assert_eq!(c["cooldown_until"], now + 1_800_000);
    assert_eq!(c["cooldown_reason"], "auth");
    assert_eq!(c["requests"], 2);
    assert_eq!(c["successes"], 1);
    assert_eq!(c["failures"], 1);
    assert_eq!(c["consecutive_failures"], 1);
    assert_eq!(c["latency_ms"], 420);
    assert_eq!(c["last_used_at"], now);
    assert_eq!(
        c["last_error"],
        json!({
            "status": 401,
            "class": "auth",
            "message": "upstream said 401",
            "at": now,
            "model": "claude-opus-4-6"
        })
    );

    // The picked OpenAI credential is the higher-priority "second".
    let o = &value[0]["credentials"][1];
    assert_eq!(o["label"], "second");
    assert_eq!(o["status"], "ready");
    assert_eq!(
        o["model_cooldowns"],
        json!([{ "model": "gpt-5.5", "until": now + 9_000, "reason": "rate_limit" }])
    );
    assert!(o.get("cooldown_until").is_none());
}

#[test]
fn credential_with_every_model_resting_counts_as_cooling() {
    let f = fixture(FULL);
    let a = f.pick("gpt-5.5").unwrap();
    f.fail(&a, &error(FailureClass::RateLimit, Some(50_000)));
    assert_eq!(
        f.credential_by_label("second").status,
        CredentialStatus::Ready
    );
    let b = f.pick("sol").unwrap();
    assert_eq!(a.credential.id, b.credential.id);
    f.fail(&b, &error(FailureClass::Server, Some(20_000)));

    let c = f.credential_by_label("second");
    assert_eq!(c.status, CredentialStatus::Cooling);
    // Until the first model comes back.
    assert_eq!(c.cooldown_until, Some(f.now_ms() + 20_000));
    assert_eq!(c.cooldown_reason, Some(FailureClass::Server));
    let models: Vec<&str> = c.model_cooldowns.iter().map(|m| m.model.as_str()).collect();
    assert_eq!(models, vec!["gpt-5.5", "gpt-6-sol"]);

    f.advance(20);
    assert_eq!(
        f.credential_by_label("second").status,
        CredentialStatus::Ready
    );
}

#[test]
fn nothing_serialisable_contains_a_secret() {
    let f = fixture(FULL);
    // Exercise every path that stores text from outside.
    for model in ["gpt-5.5", "sol", "ant/claude-opus-4-6", "smart"] {
        let lease = f.pick(model).unwrap();
        let mut failure = error(FailureClass::Server, None);
        failure.info.message = format!("bad key {}", lease.credential.api_key);
        failure.body = Some(format!("{{\"key\":\"{}\"}}", lease.credential.api_key));
        f.fail(&lease, &failure);
        f.succeed(&lease, 10);
    }
    let dump = [
        serde_json::to_string(&f.scheduler.snapshot()).unwrap(),
        serde_json::to_string(&f.scheduler.models()).unwrap(),
        serde_json::to_string(&f.scheduler.visible_models()).unwrap(),
        serde_json::to_string(&f.scheduler.warnings()).unwrap(),
        format!("{:?}", f.scheduler.snapshot()),
        format!("{:?}", f.scheduler),
    ]
    .join("\n");
    for secret in SECRETS {
        assert!(!dump.contains(secret), "{secret} leaked:\n{dump}");
    }
    assert!(!dump.contains("SECRETSECRET"));
    // The masked forms are there instead.
    assert!(dump.contains("sk-pro…-one"));
    assert!(dump.contains("[redacted]"));
}

#[test]
fn credential_snapshot_by_id() {
    let f = fixture(FULL);
    let all = f.all_credentials();
    let one = f.scheduler.credential_snapshot(&all[1].id).unwrap();
    assert_eq!(one, all[1]);
    assert!(
        f.scheduler
            .credential_snapshot("nope:000000000000")
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Model table
// ---------------------------------------------------------------------------

#[test]
fn model_table_lists_names_routes_and_availability() {
    let f = fixture(FULL);
    let table = f.scheduler.models();
    let names: Vec<&str> = table.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "ant/claude-opus-4-6",
            "claude-opus-4-6",
            "gpt-5.5",
            "smart",
            "sol"
        ]
    );

    let gpt = &table[2];
    assert_eq!(gpt.info.id, "gpt-5.5");
    assert!(gpt.info.known);
    assert!(!gpt.hidden);
    assert_eq!(gpt.alias_targets, None);
    assert_eq!(
        gpt.routes,
        vec![ModelRoute {
            provider: "openai".into(),
            upstream_model: "gpt-5.5".into(),
            credentials_total: 4,
            // Two of the four are disabled / unusable.
            credentials_available: 2,
        }]
    );

    let sol = &table[4];
    assert_eq!(sol.info.id, "sol");
    assert_eq!(sol.routes[0].upstream_model, "gpt-6-sol");

    let smart = &table[3];
    assert_eq!(
        smart.alias_targets,
        Some(vec![
            "claude-opus-4-6".to_string(),
            "gpt-5.5(high)".to_string()
        ])
    );
    assert_eq!(smart.info.id, "smart");
    // Metadata of the first target.
    assert_eq!(smart.info.context_window, Some(1_000_000));
    let routes: Vec<(&str, &str)> = smart
        .routes
        .iter()
        .map(|r| (r.provider.as_str(), r.upstream_model.as_str()))
        .collect();
    assert_eq!(
        routes,
        vec![("claude", "claude-opus-4-6"), ("openai", "gpt-5.5")]
    );
}

#[test]
fn model_table_availability_follows_cooldowns() {
    let f = fixture(FULL);
    let available = |name: &str| -> usize {
        f.scheduler
            .models()
            .into_iter()
            .find(|m| m.name == name)
            .unwrap()
            .routes[0]
            .credentials_available
    };
    assert_eq!(available("gpt-5.5"), 2);
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::Server);
    assert_eq!(available("gpt-5.5"), 1);
    // Per-model: the other model of the provider is unaffected.
    assert_eq!(available("sol"), 2);
    f.scheduler.set_runtime_disabled(&lease.credential.id, true);
    assert_eq!(available("sol"), 1);
    f.advance(60);
    assert_eq!(available("gpt-5.5"), 1);
    f.scheduler
        .set_runtime_disabled(&lease.credential.id, false);
    assert_eq!(available("gpt-5.5"), 2);
}

#[test]
fn model_table_serialises() {
    let f = fixture(FULL);
    let value = serde_json::to_value(f.scheduler.models()).unwrap();
    assert_eq!(value[2]["name"], "gpt-5.5");
    assert_eq!(value[2]["hidden"], false);
    assert!(value[2].get("alias_targets").is_none());
    assert_eq!(
        value[2]["routes"],
        json!([{
            "provider": "openai",
            "upstream_model": "gpt-5.5",
            "credentials_total": 4,
            "credentials_available": 2
        }])
    );
    assert_eq!(value[2]["info"]["id"], "gpt-5.5");
    assert_eq!(value[2]["info"]["context_window"], 272_000);
    assert_eq!(
        value[3]["alias_targets"],
        json!(["claude-opus-4-6", "gpt-5.5(high)"])
    );
}

#[test]
fn visible_models_are_sorted_unique_and_client_facing() {
    let f = fixture(FULL);
    let ids: Vec<String> = f
        .scheduler
        .visible_models()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(
        ids,
        vec![
            "ant/claude-opus-4-6",
            "claude-opus-4-6",
            "gpt-5.5",
            "smart",
            "sol"
        ]
    );
    let sol = &f.scheduler.visible_models()[4];
    assert_eq!(sol.display_name.as_deref(), Some("GPT 6.0 Sol"));
    assert_eq!(sol.owned_by.as_deref(), Some("openai"));
}

#[test]
fn same_model_from_two_providers_is_listed_once() {
    let f = fixture(
        r#"
[[providers]]
name = "one"
kind = "openai"
api_keys = ["sk-one-aaaaaaaaaaaaaaaaaaaa"]

[[providers]]
name = "two"
kind = "openai"
api_keys = ["sk-two-aaaaaaaaaaaaaaaaaaaa"]
"#,
    );
    let ids: Vec<String> = f
        .scheduler
        .visible_models()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids.iter().filter(|id| *id == "gpt-5.5").count(), 1);
    let table = f.scheduler.models();
    let gpt = table.iter().find(|m| m.name == "gpt-5.5").unwrap();
    assert_eq!(gpt.routes.len(), 2);
}

// ---------------------------------------------------------------------------
// Rebuild
// ---------------------------------------------------------------------------

const BEFORE: &str = r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-keep-aaaaaaaaaaaaaaaaaaaa", "sk-drop-aaaaaaaaaaaaaaaaaaaa"]
"#;

#[test]
fn rebuild_keeps_state_of_unchanged_credentials_and_drops_the_rest() {
    let f = fixture(BEFORE);
    let before = f.all_credentials();
    let keep = f.pick("gpt-5.5").unwrap();
    let dropped = f.pick("gpt-5.5").unwrap();
    assert_eq!(keep.credential.id, before[0].id);
    f.succeed(&keep, 250);
    f.fail_class(&keep, FailureClass::Auth);
    f.fail_class(&dropped, FailureClass::Auth);
    f.scheduler.set_runtime_disabled(&keep.credential.id, true);

    // Same key, different everything else; the second key is replaced.
    f.rebuild(
        r#"
[routing]
strategy = "weighted"

[[providers]]
name = "openai"
kind = "openai"
priority = 4
prefix = "oa"

[[providers.credentials]]
api_key = "sk-keep-aaaaaaaaaaaaaaaaaaaa"
label = "kept"
weight = 3

[[providers.credentials]]
api_key = "sk-new-aaaaaaaaaaaaaaaaaaaaa"

[[providers.models]]
id = "gpt-5.5"
"#,
    );
    let after = f.all_credentials();
    assert_eq!(after.len(), 2);

    assert_eq!(after[0].id, before[0].id);
    assert_eq!(after[0].label, "kept");
    assert_eq!(after[0].weight, 3);
    assert_eq!(after[0].priority, 4);
    assert_eq!(
        (after[0].requests, after[0].successes, after[0].failures),
        (2, 1, 1)
    );
    assert_eq!(after[0].latency_ms, Some(250));
    assert_eq!(after[0].cooldown_reason, Some(FailureClass::Auth));
    assert_eq!(after[0].cooldown_until, Some(f.now_ms() + 1_800_000));
    assert!(after[0].disabled, "runtime disable survives");
    assert_eq!(after[0].status, CredentialStatus::Disabled);
    assert!(after[0].last_error.is_some());

    // The replaced key is a new credential with a clean slate.
    assert_ne!(after[1].id, before[1].id);
    assert_eq!(after[1].requests, 0);
    assert_eq!(after[1].status, CredentialStatus::Ready);
    assert!(f.scheduler.credential_snapshot(&before[1].id).is_none());
    assert!(!f.scheduler.reset_cooldowns(&before[1].id));

    let lease = f.pick("oa/gpt-5.5").unwrap();
    assert_eq!(lease.credential.id, after[1].id);
}

#[test]
fn rebuild_applies_the_new_routing_table() {
    let f = fixture(BEFORE);
    assert!(f.scheduler.resolve("gpt-5.5").is_ok());
    assert!(f.scheduler.resolve("claude-opus-4-6").is_err());
    f.rebuild(
        r#"
[[providers]]
name = "claude"
kind = "anthropic"
api_keys = ["sk-ant-aaaaaaaaaaaaaaaaaaaa"]

[[aliases]]
name = "best"
targets = ["claude-opus-4-6"]
"#,
    );
    assert!(f.scheduler.resolve("gpt-5.5").is_err());
    assert_eq!(f.pick("best").unwrap().credential.provider, "claude");
    assert_eq!(f.scheduler.config().aliases.len(), 1);
    assert!(f.scheduler.provider_config("openai").is_none());
    assert_eq!(
        f.scheduler.provider_config("claude").unwrap().name,
        "claude"
    );
}

#[test]
fn rebuild_with_cooldowns_switched_off_clears_running_ones() {
    let f = fixture(BEFORE);
    let a = f.pick("gpt-5.5").unwrap();
    let b = f.pick("gpt-5.5").unwrap();
    f.fail_class(&a, FailureClass::Auth);
    f.fail_class(&b, FailureClass::RateLimit);
    assert!(matches!(
        f.pick("gpt-5.5"),
        Err(PickError::CoolingDown { .. })
    ));
    f.rebuild(&format!("{BEFORE}\n[routing.cooldown]\nenabled = false\n"));
    assert!(f.pick("gpt-5.5").is_ok());
    assert!(
        f.all_credentials()
            .iter()
            .all(|c| c.status == CredentialStatus::Ready && c.model_cooldowns.is_empty())
    );
    // Counters survive.
    assert_eq!(f.all_credentials()[0].failures, 1);
}

#[test]
fn rebuild_keeps_round_robin_position_and_session_bindings() {
    let f = fixture(BEFORE);
    let first = f.pick_with("gpt-5.5", &[], Some("s1")).unwrap();
    assert_eq!(first.credential.api_key, "sk-keep-aaaaaaaaaaaaaaaaaaaa");
    f.rebuild(BEFORE);
    // Rotation continues with the second key; the session stays put.
    assert_eq!(f.pick_key("gpt-5.5"), "sk-drop-aaaaaaaaaaaaaaaaaaaa");
    let again = f.pick_with("gpt-5.5", &[], Some("s1")).unwrap();
    assert_eq!(again.credential.id, first.credential.id);

    // A binding to a credential that disappeared is forgotten.
    f.rebuild(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-drop-aaaaaaaaaaaaaaaaaaaa"]
"#,
    );
    assert_eq!(f.scheduler.session_bindings(), 0);
    let moved = f.pick_with("gpt-5.5", &[], Some("s1")).unwrap();
    assert_eq!(moved.credential.api_key, "sk-drop-aaaaaaaaaaaaaaaaaaaa");

    // Switching affinity off forgets all bindings.
    f.rebuild(&format!("{BEFORE}\n[routing]\nsession_affinity = false\n"));
    assert_eq!(f.scheduler.session_bindings(), 0);
}

const COMPAT: &str = r#"
[[providers]]
name = "local"
kind = "openai-compat"
base_url = "http://localhost:11434/v1"
"#;

#[test]
fn rebuild_remembers_discovered_models_for_unchanged_providers() {
    let f = fixture(COMPAT);
    f.scheduler
        .set_discovered("local", vec![ModelInfo::bare("llama3.3")]);
    assert!(f.scheduler.resolve("llama3.3").is_ok());

    // Unrelated change: the list is kept.
    f.rebuild(&format!("{COMPAT}prefix = \"l\"\n"));
    assert!(f.scheduler.resolve("llama3.3").is_ok());
    assert!(f.scheduler.resolve("l/llama3.3").is_ok());

    // Different endpoint: the old list says nothing about it.
    f.rebuild(&COMPAT.replace("11434", "8000"));
    assert!(f.scheduler.resolve("llama3.3").is_err());
}

#[test]
fn rebuild_forgets_discovered_models_when_discovery_is_switched_off() {
    let f = fixture(COMPAT);
    f.scheduler
        .set_discovered("local", vec![ModelInfo::bare("llama3.3")]);
    f.rebuild(&format!("{COMPAT}discover = false\n"));
    assert!(f.scheduler.resolve("llama3.3").is_err());
}

#[test]
fn rebuild_accepts_fresh_discovered_lists() {
    let f = fixture(COMPAT);
    f.scheduler
        .set_discovered("local", vec![ModelInfo::bare("old-model")]);
    let lists = HashMap::from([
        ("local".to_string(), vec![ModelInfo::bare("new-model")]),
        ("ghost".to_string(), vec![ModelInfo::bare("unused")]),
    ]);
    f.scheduler.rebuild(&config(COMPAT), &resolver, lists);
    assert!(f.scheduler.resolve("new-model").is_ok());
    assert!(f.scheduler.resolve("old-model").is_err());
    assert!(f.scheduler.resolve("unused").is_err());

    // An explicit empty list forgets the remembered one.
    let lists = HashMap::from([("local".to_string(), Vec::new())]);
    f.scheduler.rebuild(&config(COMPAT), &resolver, lists);
    assert!(f.scheduler.resolve("new-model").is_err());
}

#[test]
fn mock_provider_keeps_its_models_across_rebuilds() {
    let toml = "[[providers]]\nname = \"demo\"\nkind = \"mock\"\ndiscover = false\n";
    let f = fixture(toml);
    f.scheduler
        .set_discovered("demo", vec![ModelInfo::bare("mock-echo")]);
    f.rebuild(toml);
    assert!(f.pick("mock-echo").is_ok());
}

#[test]
fn rebuild_re_resolves_secrets() {
    let toml = r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["env:LATER_KEY"]
"#;
    let f = fixture(toml);
    assert_eq!(f.all_credentials()[0].status, CredentialStatus::Unusable);
    let now_set = |value: &str| match value {
        "env:LATER_KEY" => Ok("sk-now-set-aaaaaaaaaaaaaaaa".to_string()),
        other => resolver(other),
    };
    f.scheduler.rebuild(&config(toml), &now_set, HashMap::new());
    assert_eq!(f.all_credentials()[0].status, CredentialStatus::Ready);
    assert_eq!(f.pick_key("gpt-5.5"), "sk-now-set-aaaaaaaaaaaaaaaa");
    assert!(f.scheduler.warnings().is_empty());
}

// ---------------------------------------------------------------------------
// Sharing
// ---------------------------------------------------------------------------

#[test]
fn production_constructor_uses_the_real_clock_and_resolver() {
    let cfg = config(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-literal-aaaaaaaaaaaaaaaa", "env:SWITCHYARD_SCHEDULER_TEST_UNSET_VARIABLE"]
"#,
    );
    let scheduler = Scheduler::new(&cfg, &switchyard_core::config::resolve_secret);
    let creds = &scheduler.snapshot()[0].credentials;
    assert_eq!(creds[0].status, CredentialStatus::Ready);
    assert_eq!(
        creds[1].unusable_reason.as_deref(),
        Some("environment variable SWITCHYARD_SCHEDULER_TEST_UNSET_VARIABLE is not set")
    );
    let resolved = scheduler.resolve("gpt-5.5").unwrap();
    let lease = scheduler
        .pick(&PickRequest {
            resolved: &resolved,
            tried: &[],
            session: None,
            client_protocol: Protocol::OpenaiResponses,
            now: scheduler.now(),
        })
        .unwrap();
    scheduler.report(&lease, Outcome::Success { latency_ms: 5 }, scheduler.now());
    assert_eq!(scheduler.snapshot()[0].credentials[0].successes, 1);
}

#[test]
fn concurrent_picks_and_reports_add_up() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-key-a-0000000000000", "sk-key-b-0000000000000", "sk-key-c-0000000000000", "sk-key-d-0000000000000"]
"#,
    );
    let scheduler = Arc::new(f.scheduler);
    let now = f.clock.clone();
    const THREADS: u64 = 8;
    const PER_THREAD: u64 = 500;
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let scheduler = Arc::clone(&scheduler);
            let clock = Arc::clone(&now);
            std::thread::spawn(move || {
                use switchyard_scheduler::Clock;
                let session = format!("thread-{t}");
                for i in 0..PER_THREAD {
                    let resolved = scheduler.resolve("gpt-5.5").unwrap();
                    let lease = scheduler
                        .pick(&PickRequest {
                            resolved: &resolved,
                            tried: &[],
                            session: (i % 2 == 0).then_some(session.as_str()),
                            client_protocol: Protocol::OpenaiChat,
                            now: clock.now(),
                        })
                        .unwrap();
                    scheduler.report(&lease, Outcome::Success { latency_ms: 10 }, clock.now());
                    if i % 100 == 0 {
                        let _ = scheduler.snapshot();
                        let _ = scheduler.models();
                    }
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let creds = &scheduler.snapshot()[0].credentials;
    let total: u64 = creds.iter().map(|c| c.requests).sum();
    assert_eq!(total, THREADS * PER_THREAD);
    assert!(
        creds
            .iter()
            .all(|c| c.successes == c.requests && c.requests > 0)
    );
    assert_eq!(scheduler.soonest_recovery("gpt-5.5"), None);
    assert_eq!(secs(0), std::time::Duration::ZERO);
}
