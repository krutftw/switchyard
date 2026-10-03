//! Tests of the importer on whole documents.

use super::*;
use crate::error::EXIT_USAGE;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use switchyard_core::config::{
    AliasConfig, ProviderConfig, ProviderKind, RequestLogMode, Strategy, WireApi,
};
use switchyard_core::{Effort, Protocol};

const FLAT: &str = include_str!("../../tests/fixtures/cliproxy-flat.yaml");
const NESTED: &str = include_str!("../../tests/fixtures/cliproxy-v8.yaml");

/// Every secret of the two fixtures. None may appear anywhere but in the
/// value position of the generated file.
const FIXTURE_SECRETS: &[&str] = &[
    "sk-client-flat-0001",
    "sk-client-flat-0002",
    "flat-management-secret",
    "AIzaSyFLAT-gemini-key-000000000001",
    "AIzaSyFLAT-gemini-key-000000000002",
    "AIzaSyFLAT-gemini-key-000000000003",
    "sk-ant-flat-000000000000000000001",
    "sk-flat-codex-000000000000000001",
    "sk-flat-codex-nobase-00000000001",
    "vk-flat-vertex-0000000000000001",
    "sk-or-flat-0000000000000000001",
    "sk-or-flat-0000000000000000002",
    "sk-client-v8-0001",
    "AIzaSyV8-gemini-key-00000000000001",
    "AIzaSyV8-gemini-key-00000000000002",
    "sk-ant-v8-00000000000000000000001",
    "sk-v8-codex-00000000000000000001",
    "xai-v8-key-000000000000000000001",
    "v8-oauth-client-secret-000000001",
];

fn import(text: &str) -> Imported {
    convert(text, "config.yaml").unwrap_or_else(|error| panic!("{error:?}"))
}

fn provider<'c>(config: &'c Config, name: &str) -> &'c ProviderConfig {
    config
        .provider(name)
        .unwrap_or_else(|| panic!("no provider `{name}` among {:?}", names(config)))
}

fn names(config: &Config) -> Vec<&str> {
    config.providers.iter().map(|p| p.name.as_str()).collect()
}

#[test]
fn flat_layout_general_settings() {
    let imported = import(FLAT);
    let config = &imported.config;
    assert_eq!(config.server.host, "0.0.0.0");
    assert_eq!(config.server.port, 8317);
    assert_eq!(
        config
            .auth
            .keys
            .iter()
            .map(|k| (k.name.as_str(), k.key.as_str()))
            .collect::<Vec<_>>(),
        [
            ("imported-1", "sk-client-flat-0001"),
            ("imported-2", "sk-client-flat-0002")
        ]
    );
    assert_eq!(config.admin.secret, "flat-management-secret");
    assert!(config.admin.allow_remote);
    assert_eq!(config.upstream.proxy, "socks5://127.0.0.1:1080");
    assert_eq!(config.routing.max_attempts, 4);
    assert_eq!(config.routing.strategy, Strategy::FillFirst);
    assert_eq!(config.routing.max_wait_secs, 30);
    assert_eq!(config.streaming.keepalive_secs, 20);
    assert_eq!(config.streaming.bootstrap_retries, 1);
    assert_eq!(config.logging.level, "debug");
    assert!(config.logging.file);
    assert_eq!(config.logging.request_log, RequestLogMode::All);
    assert!(!config.usage.enabled);
}

#[test]
fn flat_layout_providers_are_merged_and_named() {
    let imported = import(FLAT);
    let config = &imported.config;
    assert_eq!(
        names(config),
        [
            "gemini",
            "gemini-2",
            "anthropic",
            "openai",
            "vertex",
            "openrouter",
            "my-local-llm"
        ]
    );

    // Two keys on the default endpoint share one provider; the second
    // has a weight, so it is a credentials entry.
    let gemini = provider(config, "gemini");
    assert_eq!(gemini.kind, ProviderKind::Gemini);
    assert_eq!(gemini.base_url, "");
    assert_eq!(gemini.api_keys, ["AIzaSyFLAT-gemini-key-000000000001"]);
    assert_eq!(gemini.credentials.len(), 1);
    assert_eq!(
        gemini.credentials[0].api_key,
        "AIzaSyFLAT-gemini-key-000000000002"
    );
    assert_eq!(gemini.credentials[0].weight, Some(5));

    // A different endpoint, prefix, proxy and headers: its own provider.
    let second = provider(config, "gemini-2");
    assert_eq!(second.base_url, "https://gemini.example.com");
    assert_eq!(second.prefix, "team");
    assert_eq!(second.priority, 10);
    assert_eq!(second.proxy, "direct");
    assert_eq!(
        second.headers.get("X-Custom-Header").map(String::as_str),
        Some("custom-value")
    );
    assert_eq!(second.headers.len(), 1, "the `$` header is dropped");
    assert_eq!(second.exclude, ["gemini-2.5-pro", "*-preview"]);
    assert_eq!(second.models.len(), 1);
    assert_eq!(second.models[0].id, "gemini-2.5-flash");
    assert_eq!(second.models[0].alias, "gemini-flash");
    assert_eq!(second.models[0].display_name, "Gemini Flash");
    assert_eq!(second.models[0].context_window, Some(1_048_576));
    let thinking = second.models[0].thinking.as_ref().unwrap();
    assert_eq!(thinking.levels, [Effort::High, Effort::Medium, Effort::Low]);
    assert!(thinking.zero_allowed);
    assert!(thinking.dynamic_allowed);

    let anthropic = provider(config, "anthropic");
    assert_eq!(anthropic.kind, ProviderKind::Anthropic);
    assert_eq!(anthropic.api_keys, ["sk-ant-flat-000000000000000000001"]);

    let openai = provider(config, "openai");
    assert_eq!(openai.kind, ProviderKind::Openai);
    assert_eq!(openai.wire_api, WireApi::Responses);
    assert_eq!(openai.base_url, "https://api.openai.com/v1");
    assert_eq!(openai.api_keys, ["sk-flat-codex-000000000000000001"]);

    let vertex = provider(config, "vertex");
    assert_eq!(vertex.kind, ProviderKind::Vertex);
    assert_eq!(vertex.api_keys, ["vk-flat-vertex-0000000000000001"]);
    assert_eq!(vertex.base_url, "https://vertex.example.com/api");
}

#[test]
fn flat_layout_openai_compatibility() {
    let imported = import(FLAT);
    let config = &imported.config;
    let openrouter = provider(config, "openrouter");
    assert_eq!(openrouter.kind, ProviderKind::OpenaiCompat);
    assert_eq!(openrouter.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(openrouter.prefix, "or");
    assert_eq!(openrouter.priority, 3);
    assert_eq!(
        openrouter.headers.get("X-Title").map(String::as_str),
        Some("switchyard")
    );
    // The key with a proxy is a credentials entry, the plain one is not.
    assert_eq!(openrouter.api_keys, ["sk-or-flat-0000000000000000002"]);
    assert_eq!(openrouter.credentials.len(), 1);
    assert_eq!(
        openrouter.credentials[0].api_key,
        "sk-or-flat-0000000000000000001"
    );
    assert_eq!(
        openrouter.credentials[0].proxy,
        "socks5://proxy.example.com:1080"
    );
    assert_eq!(openrouter.credentials[0].weight, Some(5));
    // One aliased model and a pool of two.
    let ids: Vec<(&str, &str)> = openrouter
        .models
        .iter()
        .map(|m| (m.id.as_str(), m.alias.as_str()))
        .collect();
    assert_eq!(
        ids,
        [
            ("moonshotai/kimi-k2:free", "kimi-k2"),
            ("deepseek-v3.1", ""),
            ("glm-5", "")
        ]
    );
    assert_eq!(
        config.aliases,
        vec![AliasConfig {
            name: "big-pool".into(),
            targets: vec!["or/deepseek-v3.1".into(), "or/glm-5".into()],
            hide_targets: true,
        }]
    );

    // A keyless local server, with a name that needs sanitising.
    let local = provider(config, "my-local-llm");
    assert_eq!(local.base_url, "http://127.0.0.1:11434/v1");
    assert!(local.api_keys.is_empty() && local.credentials.is_empty());
    assert!(!local.enabled);
}

#[test]
fn flat_layout_payload_rules() {
    let imported = import(FLAT);
    let payload = &imported.config.payload;
    assert_eq!(payload.default.len(), 1);
    assert_eq!(payload.default[0].models, ["gemini-*"]);
    assert_eq!(payload.default[0].protocol, Some(Protocol::Gemini));
    assert_eq!(
        payload.default[0]
            .set
            .get("generationConfig.thinkingConfig.thinkingBudget"),
        Some(&serde_json::json!(32768))
    );
    // The raw rule's JSON text is parsed; it applies to two protocols.
    assert_eq!(payload.overrides.len(), 2);
    assert_eq!(
        payload.overrides[0].protocol,
        Some(Protocol::OpenaiResponses)
    );
    assert_eq!(payload.overrides[0].models, ["gpt-*"]);
    assert_eq!(payload.overrides[1].protocol, None);
    assert_eq!(payload.overrides[1].models, ["o3*"]);
    assert_eq!(
        payload.overrides[0].set.get("reasoning"),
        Some(&serde_json::json!({"summary": "auto"}))
    );
    // The filter keeps its plain path and loses the gjson query.
    assert_eq!(payload.filter.len(), 1);
    assert_eq!(payload.filter[0].remove, ["metadata"]);
    let report = imported.not_imported.join("\n");
    assert!(
        report.contains("payload.filter[0]: 1 of the paths use gjson queries"),
        "{report}"
    );
    assert!(
        report.contains("payload.default[1]: 1 of the model entries have conditions"),
        "{report}"
    );
}

#[test]
fn flat_layout_report() {
    let imported = import(FLAT);
    let report = imported.not_imported.join("\n");
    for needle in [
        "OAuth logins and credential files",
        "auth-dir",
        "client impersonation is deliberately not supported",
        "claude-api-key[].cloak.mode",
        "plugins are not supported",
        "the Amp integration is not supported",
        "the Amp integration is not supported: ampcode",
        "quota-exceeded switches",
        "(upstream errors are classified by the gateway): gemini-api-key[].request-scoped-errors",
        "per-credential retry and cooldown overrides",
        "a codex key without base-url",
        "relaying to an upstream's Responses WebSocket is not supported",
        "the upstream is reached over HTTP streaming): codex-api-key[].websockets",
        "gemini-api-key[2]: 1 of the headers copy their value from the client request",
        "settings this importer does not know: some-future-setting",
    ] {
        assert!(report.contains(needle), "missing `{needle}` in:\n{report}");
    }
    let notes = imported.notes.join("\n");
    assert!(notes.contains("the model pool `big-pool`"), "{notes}");
}

/// The upstream Responses WebSocket relay does not exist here: a key that
/// switches it on is imported like any other and the switch is reported,
/// in both layouts. A switch that is off says nothing.
#[test]
fn upstream_websocket_switch_is_reported_not_imported() {
    let flat = import(
        "codex-api-key:\n  - api-key: sk-codex-ws-000000000001\n    base-url: https://api.openai.com/v1\n    websockets: true\n  - api-key: sk-codex-ws-000000000002\n    base-url: https://api.openai.com/v1\n    websockets: false\n",
    );
    // The switch no longer splits the keys into two providers.
    assert_eq!(names(&flat.config), ["openai"]);
    assert_eq!(
        provider(&flat.config, "openai").api_keys,
        ["sk-codex-ws-000000000001", "sk-codex-ws-000000000002"]
    );
    let report = flat.not_imported.join("\n");
    assert!(
        report.contains("Responses WebSocket is not supported")
            && report.contains("codex-api-key[].websockets"),
        "{report}"
    );
    assert!(!flat.text.contains("websocket ="), "{}", flat.text);

    let nested = import(
        "api-keys:\n  codex:\n    - base-url: https://api.openai.com/v1\n      keys:\n        - api-key: sk-codex-ws-000000000003\n          websockets: true\n",
    );
    let report = nested.not_imported.join("\n");
    assert!(
        report.contains("HTTP streaming): api-keys.codex[].keys[].websockets"),
        "{report}"
    );

    let off = import(
        "codex-api-key:\n  - api-key: sk-codex-ws-000000000004\n    base-url: https://api.openai.com/v1\n    websockets: false\n",
    );
    assert!(off.not_imported.is_empty(), "{:?}", off.not_imported);
}

/// What the configuration refuses is left out and said, never written into
/// a file that would then not load.
#[test]
fn values_the_stricter_validation_refuses_are_left_out_and_reported() {
    let imported = import(
        "host: \"a..b\"\n\
         api-keys: [\"env:\", plain-client-key-0001, \"${}\"]\n\
         remote-management: {secret-key: \"env:\"}\n\
         gemini-api-key:\n  - api-key: \"env:\"\n  - api-key: gm-plain-0000000001\n\
         openai-compatibility:\n  - name: pools\n    base-url: https://pools.example/v1\n    api-key-entries:\n      - api-key: \"${ }\"\n      - api-key: sk-pools-0000000001\n    models:\n      - {name: up-a, alias: \"my pool\"}\n      - {name: up-b, alias: \"my pool\"}\n      - {name: up-c, alias: \"deep(high)\"}\n      - {name: up-d, alias: \"deep(high)\"}\n      - {name: up-e, alias: ok-pool}\n      - {name: up-f, alias: ok-pool}\n      - {name: up-g, thinking: {levels: [low, high], min: 5000, max: 100}}\n",
    );
    let config = &imported.config;
    assert!(config.validate().is_empty());
    assert_eq!(config.server.host, "127.0.0.1");
    assert_eq!(client_keys(config), ["plain-client-key-0001"]);
    assert_eq!(config.admin.secret, "");
    assert_eq!(provider(config, "gemini").api_keys, ["gm-plain-0000000001"]);
    let pools = provider(config, "pools");
    assert_eq!(pools.api_keys, ["sk-pools-0000000001"]);
    // Only the pool with a usable name became a virtual model; of the
    // others the first model keeps the name.
    assert_eq!(
        config
            .aliases
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        ["ok-pool"]
    );
    let models: Vec<(&str, &str)> = pools
        .models
        .iter()
        .map(|m| (m.id.as_str(), m.alias.as_str()))
        .collect();
    assert_eq!(
        models,
        [
            ("up-a", "my pool"),
            ("up-c", "deep(high)"),
            ("up-e", ""),
            ("up-f", ""),
            ("up-g", "")
        ]
    );
    let thinking = pools.models[4].thinking.as_ref().expect("levels kept");
    assert_eq!(thinking.levels, [Effort::Low, Effort::High]);
    assert_eq!((thinking.min, thinking.max), (0, 0));

    let report = imported.not_imported.join("\n");
    for needle in [
        "host: not an address or a host name",
        "2 of the client API keys: written as an environment reference",
        "the management secret-key: written as an environment reference",
        "gemini-api-key[0]: the api-key is written as an environment reference",
        "provider `pools`: 1 of the keys are written as an environment reference",
        "the model pool `my pool` cannot become a virtual model",
        "the model pool `deep(high)` cannot become a virtual model",
        "1 of the models have a thinking range whose min is above its max",
    ] {
        assert!(report.contains(needle), "missing `{needle}` in:\n{report}");
    }
    assert_eq!(&validate_text(&imported.text).unwrap(), config);
}

#[test]
fn nested_layout() {
    let imported = import(NESTED);
    let config = &imported.config;
    assert_eq!(config.server.host, "127.0.0.1");
    assert_eq!(config.server.port, 9000);
    assert_eq!(config.auth.keys.len(), 1);
    assert_eq!(config.auth.keys[0].key, "sk-client-v8-0001");
    // A bcrypt hash cannot be imported.
    assert_eq!(config.admin.secret, "");
    assert!(!config.admin.allow_remote);
    assert_eq!(config.routing.strategy, Strategy::Weighted);
    assert!(!config.routing.session_affinity);
    assert_eq!(config.routing.session_affinity_ttl_secs, 5400);
    assert!(config.routing.force_model_prefix);
    assert_eq!(config.routing.max_attempts, 1);
    assert!(!config.routing.cooldown.enabled);
    assert_eq!(config.routing.cooldown.transient_secs, 15);
    assert_eq!(config.upstream.proxy, "");
    assert!(!config.upstream.passthrough_headers);
    assert_eq!(config.streaming.keepalive_secs, 0);
    assert_eq!(config.logging.level, "info");
    assert_eq!(config.logging.request_log, RequestLogMode::Off);
    assert!(config.usage.enabled);
    let tls = config.server.tls.as_ref().expect("tls imported");
    assert_eq!(
        (tls.cert.as_str(), tls.key.as_str()),
        ("cert.pem", "key.pem")
    );

    assert_eq!(
        names(config),
        ["gemini", "gemini-2", "anthropic", "openai", "groq"]
    );

    // The first key inherits everything from its group.
    let gemini = provider(config, "gemini");
    assert_eq!(gemini.prefix, "team");
    assert_eq!(gemini.proxy, "socks5://proxy.example.com:1080");
    assert_eq!(gemini.exclude, ["*-preview"]);
    assert_eq!(gemini.credentials.len(), 1);
    assert_eq!(gemini.credentials[0].weight, Some(5));
    assert_eq!(
        gemini.credentials[0].api_key,
        "AIzaSyV8-gemini-key-00000000000001"
    );
    // The second overrides the proxy and clears the exclusions, so it
    // cannot share the provider.
    let second = provider(config, "gemini-2");
    assert_eq!(second.prefix, "team", "null inherits");
    assert_eq!(second.proxy, "direct");
    assert!(
        second.exclude.is_empty(),
        "an explicit empty list overrides"
    );
    assert_eq!(second.api_keys, ["AIzaSyV8-gemini-key-00000000000002"]);
    assert_eq!(second.headers, gemini.headers);

    assert_eq!(
        provider(config, "anthropic").api_keys,
        ["sk-ant-v8-00000000000000000000001"]
    );
    let openai = provider(config, "openai");
    assert_eq!(openai.wire_api, WireApi::Responses);
    assert_eq!(openai.base_url, "https://codex.example.com/v1");

    let groq = provider(config, "groq");
    assert_eq!(groq.kind, ProviderKind::OpenaiCompat);
    assert!(groq.api_keys.is_empty() && groq.credentials.is_empty());

    let report = imported.not_imported.join("\n");
    for needle in [
        "the management secret-key: it is stored as a bcrypt hash",
        "can be added by hand as an openai-compat provider: api-keys.xai",
        "client impersonation is deliberately not supported",
        "api-keys.claude[].keys[].fingerprint-profile",
        "OAuth logins and credential files",
        "multimedia settings",
    ] {
        assert!(report.contains(needle), "missing `{needle}` in:\n{report}");
    }
    // The flat spellings that the nested ones override are not reported
    // as unknown.
    assert!(
        !report.contains("settings this importer does not know"),
        "{report}"
    );
}

#[test]
fn nested_spelling_wins_by_presence() {
    let imported = import(
        "port: 1111\nserver:\n  port: 2222\nrequest-retry: 4\nrouting:\n  retry:\n    max-retry-credentials: 2\ndebug: true\nobservability:\n  logs:\n    debug: false\napi-keys:\n  gemini: []\ngemini-api-key:\n  - api-key: ignored-flat-twin-000000\n",
    );
    assert_eq!(imported.config.server.port, 2222);
    // No nested `request-retry` leaf: the flat one counts.
    assert_eq!(imported.config.routing.max_attempts, 5);
    assert_eq!(imported.config.logging.level, "info");
    assert!(imported.config.providers.is_empty());
}

#[test]
fn generated_file_validates_and_reads_back() {
    for fixture in [FLAT, NESTED] {
        let imported = import(fixture);
        let parsed = validate_text(&imported.text).expect("generated file is valid");
        assert_eq!(parsed, imported.config);
        assert!(
            imported
                .text
                .starts_with("# Switchyard configuration, imported from config.yaml")
        );
        // The report is in the header, as comments.
        assert!(imported.text.contains("#\n# Not imported:\n#   - "));
        for line in &imported.not_imported {
            assert!(imported.text.contains(&format!("#   - {line}\n")), "{line}");
        }
        for line in &imported.notes {
            assert!(imported.text.contains(&format!("#   - {line}\n")), "{line}");
        }
    }
}

#[test]
fn keys_on_one_endpoint_share_a_provider() {
    let imported = import(
        "\
gemini-api-key:
  - api-key: key-a-0000000000000000
  - api-key: key-b-0000000000000000
    weight: 1
  - api-key: key-a-0000000000000000
  - api-key: key-c-0000000000000000
    base-url: https://generativelanguage.googleapis.com/
  - api-key: key-d-0000000000000000
    proxy-url: direct
claude-api-key:
  - api-key: key-e-0000000000000000
    priority: 5
  - api-key: key-f-0000000000000000
",
    );
    let config = &imported.config;
    assert_eq!(names(config), ["gemini", "gemini-2", "anthropic"]);

    // The default endpoint written out is still the default endpoint, a
    // weight of 1 is the default weight, and a repeated key counts once.
    let gemini = provider(config, "gemini");
    assert_eq!(gemini.base_url, "");
    assert_eq!(
        gemini.api_keys,
        [
            "key-a-0000000000000000",
            "key-b-0000000000000000",
            "key-c-0000000000000000"
        ]
    );
    assert!(gemini.credentials.is_empty());
    assert!(
        imported
            .notes
            .iter()
            .any(|note| note.contains("provider `gemini`: keys listed more than once")),
        "{:?}",
        imported.notes
    );

    // Another proxy is another provider.
    let direct = provider(config, "gemini-2");
    assert_eq!(direct.proxy, "direct");
    assert_eq!(direct.api_keys, ["key-d-0000000000000000"]);

    // Another priority is not: the first entry gives the provider's, the
    // other key overrides it.
    let anthropic = provider(config, "anthropic");
    assert_eq!(anthropic.priority, 5);
    assert_eq!(anthropic.api_keys, ["key-e-0000000000000000"]);
    assert_eq!(anthropic.credentials.len(), 1);
    assert_eq!(anthropic.credentials[0].api_key, "key-f-0000000000000000");
    assert_eq!(anthropic.credentials[0].priority, Some(0));
    assert_eq!(anthropic.credentials[0].weight, None);
    assert_eq!(validate_text(&imported.text).unwrap(), imported.config);
}

#[test]
fn generated_file_leaves_defaults_out() {
    let imported = import("port: 8317\napi-keys:\n  - sk-client-0000000000000001\n");
    let body: Vec<&str> = imported
        .text
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .collect();
    assert_eq!(
        body,
        [
            "[server]",
            "host = \"0.0.0.0\"",
            "port = 8317",
            "[[auth.keys]]",
            "key = \"sk-client-0000000000000001\"",
            "name = \"imported-1\"",
        ]
    );
}

#[test]
fn secrets_appear_only_as_values_of_the_generated_file() {
    for fixture in [FLAT, NESTED] {
        let imported = import(fixture);
        let commentary: String = imported
            .text
            .lines()
            .filter(|line| line.starts_with('#'))
            .chain(imported.not_imported.iter().map(String::as_str))
            .chain(imported.notes.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        for secret in FIXTURE_SECRETS {
            assert!(
                !commentary.contains(secret),
                "{secret} leaked:\n{commentary}"
            );
        }
        assert!(
            !commentary.contains("$2a$"),
            "the hash leaked:\n{commentary}"
        );
    }
}

#[test]
fn run_writes_the_file_and_prints_no_secret() {
    let dir = tempfile::tempdir().unwrap();
    let no_env = |_: &str| None;
    for (name, fixture) in [("flat", FLAT), ("v8", NESTED)] {
        let input = dir.path().join(format!("{name} config.yaml"));
        std::fs::write(&input, fixture).unwrap();
        let output = dir.path().join("out dir").join(format!("{name}.toml"));
        let args = ImportArgs {
            input: input.clone(),
            output: output.clone(),
            force: false,
        };
        let printed = run(&args, &no_env).unwrap();
        let written = std::fs::read_to_string(&output).unwrap();
        validate_text(&written).unwrap();
        assert!(
            printed.stdout.starts_with("imported "),
            "{}",
            printed.stdout
        );
        assert!(
            printed.stdout.contains("providers    "),
            "{}",
            printed.stdout
        );
        assert!(
            printed.stdout.contains("payload rules "),
            "{}",
            printed.stdout
        );
        assert!(
            printed.stderr.starts_with("not imported:\n  - "),
            "{}",
            printed.stderr
        );
        for secret in FIXTURE_SECRETS {
            assert!(!printed.stdout.contains(secret), "{secret} on stdout");
            assert!(!printed.stderr.contains(secret), "{secret} on stderr");
        }

        // A second run refuses to overwrite, and --force replaces.
        let refused = run(&args, &no_env).unwrap_err();
        assert_eq!(refused.code, EXIT_USAGE);
        assert!(refused.message.contains("--force"));
        let forced = ImportArgs {
            force: true,
            ..args
        };
        run(&forced, &no_env).unwrap();
    }
}

#[test]
fn run_rejects_bad_input() {
    let dir = tempfile::tempdir().unwrap();
    let no_env = |_: &str| None;
    let args = |input: PathBuf| ImportArgs {
        input,
        output: dir.path().join("out.toml"),
        force: false,
    };

    let missing = run(&args(dir.path().join("nope.yaml")), &no_env).unwrap_err();
    assert_eq!(missing.code, EXIT_USAGE);

    let broken = dir.path().join("broken.yaml");
    std::fs::write(&broken, "port: [1, 2\nsecret: hunter2-do-not-echo\n").unwrap();
    let error = run(&args(broken), &no_env).unwrap_err();
    assert_eq!(error.code, EXIT_USAGE);
    assert!(error.message.contains("line "), "{}", error.message);
    assert!(!error.message.contains("hunter2"), "{}", error.message);

    let list = dir.path().join("list.yaml");
    std::fs::write(&list, "- a\n- b\n").unwrap();
    let error = run(&args(list), &no_env).unwrap_err();
    assert!(
        error.message.contains("must be a mapping"),
        "{}",
        error.message
    );

    let binary = dir.path().join("binary.yaml");
    std::fs::write(&binary, [0xff, 0xfe, 0xfd]).unwrap();
    assert_eq!(run(&args(binary), &no_env).unwrap_err().code, EXIT_USAGE);

    assert_eq!(
        run(&args(dir.path().to_path_buf()), &no_env)
            .unwrap_err()
            .code,
        EXIT_USAGE
    );
    assert!(!dir.path().join("out.toml").exists());

    // The input is never overwritten, even with --force.
    let same = dir.path().join("same.yaml");
    std::fs::write(&same, "port: 1\n").unwrap();
    let error = run(
        &ImportArgs {
            input: same.clone(),
            output: same.clone(),
            force: true,
        },
        &no_env,
    )
    .unwrap_err();
    assert_eq!(error.code, EXIT_USAGE);
    assert_eq!(std::fs::read_to_string(&same).unwrap(), "port: 1\n");
}

#[test]
fn an_empty_document_gives_a_valid_default_file() {
    for text in ["", "# nothing\n", "{}\n"] {
        let imported = import(text);
        assert!(imported.config.providers.is_empty());
        assert!(imported.config.auth.keys.is_empty());
        validate_text(&imported.text).unwrap();
    }
}

#[test]
fn hostile_values_cannot_break_the_generated_file() {
    let text = "\
port: 99999
host: \"evil\\nhost\"
proxy-url: \"ftp://nope\"
api-keys: [\"  \", 7, \"dup\", \"dup\", \"your-api-key-1\"]
\"weird\\nkey = 1\\n[admin]\\nsecret\": injected
remote-management:
  secret-key: \"quote\\\" and \\\\ backslash\\nnewline\"
gemini-api-key:
  - api-key: \"k\\\"1\"
    base-url: \"javascript:alert(1)\"
  - api-key: \"key-with-odd-proxy-0001\"
    proxy-url: \"not a url\"
    weight: 1.5
    prefix: \"a/b\"
    priority: 99999999999
  - not-a-mapping
  - api-key: \"key-with-big-weight-001\"
    weight: 5000000
openai-compatibility:
  - name: \"Bad Name!! / ✓\"
    base-url: \"https://x.example/v1\"
    headers:
      \"Bad Header\": \"x\"
      \"X-Ok\": \"fine\"
      \"X-Ctl\": \"a\\tb\"
    models:
      - name: m
        alias: m
      - name: m
        alias: m
      - alias: only-alias
  - name: \"\"
    base-url: \"https://y.example/v1\"
  - name: \"bad name\"
    base-url: \"https://z.example/v1\"
  - base-url: \"\"
payload:
  default:
    - models: [{name: \"x\"}]
      params: {\"a.b\": null, \"c\": 1}
  override-raw:
    - models: [{name: \"x\"}]
      params: {\"d\": \"{not json\"}
  bogus: []
";
    let imported = import(text);
    let config = &imported.config;
    assert_eq!(config.server.port, 8317);
    assert_eq!(config.server.host, "127.0.0.1");
    assert_eq!(config.upstream.proxy, "");
    assert_eq!(
        config
            .auth
            .keys
            .iter()
            .map(|k| k.key.as_str())
            .collect::<Vec<_>>(),
        ["7", "dup"]
    );
    // A secret with a line break cannot travel in a header, and the file
    // would be refused (A2-4): it is reported, not imported.
    assert_eq!(config.admin.secret, "");
    assert!(
        imported.not_imported.iter().any(|line| line
            .starts_with("the management secret-key: an admin secret must not contain control")),
        "{:?}",
        imported.not_imported
    );
    assert_eq!(
        names(config),
        ["gemini", "bad-name", "openai-compat", "bad-name-2"]
    );
    // Both surviving keys share the default endpoint. The first sets the
    // provider's priority; its fractional weight is ignored.
    let gemini = provider(config, "gemini");
    assert_eq!(gemini.proxy, "");
    assert_eq!(gemini.prefix, "");
    assert_eq!(gemini.priority, i32::MAX);
    assert_eq!(gemini.api_keys, ["key-with-odd-proxy-0001"]);
    assert_eq!(gemini.credentials.len(), 1);
    assert_eq!(gemini.credentials[0].weight, Some(1_000_000));
    assert_eq!(gemini.credentials[0].priority, Some(0));
    let compat = provider(config, "bad-name");
    assert_eq!(compat.models.len(), 1);
    assert_eq!(
        compat.headers.iter().collect::<Vec<_>>(),
        [(&"X-Ok".to_string(), &"fine".to_string())]
    );
    assert!(
        imported
            .not_imported
            .iter()
            .any(|line| line.contains("2 of the headers are not valid HTTP headers")),
        "{:?}",
        imported.not_imported
    );
    assert_eq!(config.payload.default.len(), 1);
    assert_eq!(config.payload.default[0].set.len(), 1);
    assert!(config.payload.overrides.is_empty());

    // The file is still exactly this configuration, and no line of the
    // header escaped its comment.
    let parsed = validate_text(&imported.text).unwrap();
    assert_eq!(&parsed, config);
    let header_end = imported.text.find("\n\n").unwrap();
    assert!(
        imported.text[..header_end]
            .lines()
            .all(|line| line.starts_with('#'))
    );
}

fn client_keys(config: &Config) -> Vec<&str> {
    config.auth.keys.iter().map(|k| k.key.as_str()).collect()
}

/// The source program reads these settings as text and gets the scalar as
/// it stands in the file. Typing `012345` as a number on the way would
/// import a different key.
#[test]
fn unquoted_text_that_looks_like_a_number_is_imported_as_written() {
    let flat = "\
port: 8317
remote-management:
  secret-key: 012345
api-keys:
  - 00998877
  - 123456789012345678901234567890
  - 1e5
  - 0x1F
  - 8317
  - 2.50
  - true
  - ~
proxy-url: 0
gemini-api-key:
  - api-key: 0123456789
    prefix: 007
    headers:
      X-Build: 010
      X-Ratio: 1.50
    excluded-models:
      - 1.50
      - 0o7
claude-api-key:
  - api-key: 1e10
openai-compatibility:
  - name: 007
    base-url: \"https://llm.example.com/v1\"
    api-key-entries:
      - api-key: 0x00ff
      - api-key: +1234
        weight: 0x10
    models:
      - name: 1.50
        alias: 0042
        display-name: 1e2
";
    let imported = import(flat);
    let config = &imported.config;
    assert_eq!(config.admin.secret, "012345");
    assert_eq!(
        client_keys(config),
        [
            "00998877",
            "123456789012345678901234567890",
            "1e5",
            "0x1F",
            "8317",
            "2.50",
            "true"
        ]
    );
    let gemini = provider(config, "gemini");
    assert_eq!(gemini.api_keys, ["0123456789"]);
    assert_eq!(gemini.prefix, "007");
    assert_eq!(
        gemini
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect::<Vec<_>>(),
        [("X-Build", "010"), ("X-Ratio", "1.50")]
    );
    assert_eq!(gemini.exclude, ["1.50", "0o7"]);
    assert_eq!(provider(config, "anthropic").api_keys, ["1e10"]);
    let compat = provider(config, "007");
    assert_eq!(compat.api_keys, ["0x00ff"]);
    assert_eq!(compat.credentials.len(), 1);
    assert_eq!(compat.credentials[0].api_key, "+1234");
    // Where the setting is a number, every spelling of one is read.
    assert_eq!(compat.credentials[0].weight, Some(16));
    assert_eq!(compat.models.len(), 1);
    assert_eq!(compat.models[0].id, "1.50");
    assert_eq!(compat.models[0].alias, "0042");
    assert_eq!(compat.models[0].display_name, "1e2");

    // The file says the same, and reads back as the same configuration.
    for written in [
        "secret = \"012345\"",
        "\"00998877\"",
        "\"123456789012345678901234567890\"",
        "\"1e5\"",
        "\"0x1F\"",
        "\"0123456789\"",
        "\"0x00ff\"",
        "\"+1234\"",
    ] {
        assert!(imported.text.contains(written), "{written}");
    }
    assert_eq!(&validate_text(&imported.text).unwrap(), config);

    let nested = "\
access:
  api-keys:
    - 00998877
    - 1_000
management:
  secret-key: 0600
api-keys:
  gemini:
    - name: 01
      prefix: 0x0A
      keys:
        - api-key: 0000000000000000000001
        - api-key: 1.0e3
  openai-compatibility:
    - name: 1e3
      base-url: \"https://llm.example.com/v1\"
      keys:
        - api-key: 000
";
    let imported = import(nested);
    let config = &imported.config;
    assert_eq!(client_keys(config), ["00998877", "1_000"]);
    assert_eq!(config.admin.secret, "0600");
    let gemini = provider(config, "gemini");
    assert_eq!(gemini.api_keys, ["0000000000000000000001", "1.0e3"]);
    assert_eq!(gemini.prefix, "0x0A");
    assert_eq!(provider(config, "1e3").api_keys, ["000"]);
    assert_eq!(&validate_text(&imported.text).unwrap(), config);
}

/// Numbers and switches are read in the spellings the source program
/// takes for them, whichever way the document is read.
#[test]
fn numbers_and_switches_in_other_spellings() {
    let text = "\
port: 0x20FD
debug: yes
logging-to-file: On
usage-statistics-enabled: no
request-retry: 03
max-retry-interval: 1e2
streaming:
  keepalive-seconds: +15
  bootstrap-retries: 2.0
remote-management:
  allow-remote: True
gemini-api-key:
  - api-key: gm-key-0000000000000001
    priority: -0x02
    weight: 1_0
";
    let imported = import(text);
    let config = &imported.config;
    assert_eq!(config.server.port, 0x20FD);
    assert_eq!(config.logging.level, "debug");
    assert!(config.logging.file);
    assert!(!config.usage.enabled);
    assert_eq!(config.routing.max_attempts, 4);
    assert_eq!(config.routing.max_wait_secs, 100);
    assert_eq!(config.streaming.keepalive_secs, 15);
    assert_eq!(config.streaming.bootstrap_retries, 2);
    assert!(config.admin.allow_remote);
    let gemini = provider(config, "gemini");
    assert_eq!(gemini.priority, -2);
    assert_eq!(gemini.credentials[0].weight, Some(10));
    assert!(
        imported.not_imported.is_empty(),
        "{:?}",
        imported.not_imported
    );
}

/// What a payload rule sets is a JSON value of any type, so there (and
/// only there) an unquoted scalar is typed; the names around it are text.
#[test]
fn payload_values_are_typed_and_names_are_not() {
    // (The first line is written out: a line continuation would eat its
    // indentation.)
    let rules = "  default:
    - models:
        - name: 1.50
          protocol: gemini
        - 0042
      params:
        temperature: 1.50
        top_k: 0x10
        stream: True
        seed: 007
        big: 1e3
        label: \"1.50\"
        word: plain
        nested: {a: 010, b: [1.0, \"2\"]}
  override-raw:
    - models: [{name: m}]
      params:
        limits: '{\"max\": 010.5}'
        count: 012
  filter:
    - models: [{name: 2.0}]
      params: [007, metadata.1.50]
";
    let expected_set = serde_json::json!({
        "temperature": 1.5,
        "top_k": 16,
        "stream": true,
        "seed": 7,
        "big": 1000.0,
        "label": "1.50",
        "word": "plain",
        "nested": {"a": 10, "b": [1.0, "2"]}
    });
    for text in [
        format!("payload:\n{rules}"),
        // The nested layout has the same rules one level down.
        format!(
            "requests:\n  payload:\n{}",
            rules
                .lines()
                .map(|line| format!("  {line}\n"))
                .collect::<String>()
        ),
    ] {
        let imported = import(&text);
        let payload = &imported.config.payload;
        assert_eq!(payload.default.len(), 2, "{:?}", imported.not_imported);
        assert_eq!(payload.default[0].protocol, Some(Protocol::Gemini));
        assert_eq!(payload.default[0].models, ["1.50"]);
        assert_eq!(payload.default[1].protocol, None);
        assert_eq!(payload.default[1].models, ["0042"]);
        assert_eq!(
            serde_json::to_value(&payload.default[0].set).unwrap(),
            expected_set
        );
        assert_eq!(payload.overrides.len(), 1);
        // JSON text that is not JSON is reported, a bare number is taken.
        assert_eq!(
            serde_json::to_value(&payload.overrides[0].set).unwrap(),
            serde_json::json!({"count": 12})
        );
        assert_eq!(payload.filter.len(), 1);
        assert_eq!(payload.filter[0].models, ["2.0"]);
        assert_eq!(payload.filter[0].remove, ["007", "metadata.1.50"]);
        assert_eq!(&validate_text(&imported.text).unwrap(), &imported.config);
    }
}

/// Importing must not turn literal source secrets into host environment access.
#[test]
fn secrets_that_read_as_environment_references_are_left_out() {
    let imported = import(
        "api-keys: [\"env:CLIENT_SECRET_NAME\", plain-client-key-0001]\n\
         remote-management: {secret-key: \"${ADMIN_SECRET_NAME}\"}\n\
         gemini-api-key:\n  - api-key: \"env:UPSTREAM_SECRET_NAME\"\n  - api-key: gm-plain-0000000001\n",
    );
    assert_eq!(imported.config.auth.keys.len(), 1);
    assert_eq!(imported.config.auth.keys[0].key, "plain-client-key-0001");
    assert!(imported.config.admin.secret.is_empty());
    assert_eq!(
        imported.config.providers[0].api_keys,
        ["gm-plain-0000000001"]
    );
    let report = imported.not_imported.join("\n");
    assert!(report.contains("client API keys: written as an environment reference"));
    assert!(report.contains("management secret-key: written as an environment reference"));
    assert!(report.contains("gemini-api-key[0]"));
    for name in [
        "CLIENT_SECRET_NAME",
        "ADMIN_SECRET_NAME",
        "UPSTREAM_SECRET_NAME",
    ] {
        assert!(!imported.text.contains(name));
    }
}

/// Deterministic pseudo-random numbers (xorshift64*), for the test below.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'t>(&mut self, items: &[&'t str]) -> &'t str {
        items[self.below(items.len())]
    }
}

/// Scalars in every spelling, as YAML flow values.
const ODD_SCALARS: &[&str] = &[
    "true",
    "false",
    "True",
    "yes",
    "off",
    "~",
    "null",
    "0",
    "1",
    "-3",
    "007",
    "0x1F",
    "0o17",
    "1e5",
    "1.50",
    "+7",
    "1_000",
    ".inf",
    "99999999999999999999",
    "65536",
    "\"\"",
    "\"quoted text\"",
    "'8317'",
    "plain",
    "a/b",
    "\"/a/\"",
    "\"$Header\"",
    "\"http://127.0.0.1:1\"",
    "\"https://example.com/v1/\"",
    "\"socks5://u:p@h:1\"",
    "\"direct\"",
    "\"env:HOME\"",
    "\"${HOME}\"",
    "\"$2a$10$abcdefghijklmnopqrstuv\"",
    "round-robin",
    "fill-first",
    "1h30m",
    "[]",
    "{}",
    "[a, 007, true, ~, 1.50]",
    "{name: 007, alias: 1e5}",
    "{X-A: 010, \"Bad Header\": x}",
    "[{name: m, alias: 0042}, {name: 1.50}, {alias: x}, 7]",
    "{levels: [low, 3, none], min: 0x10, max: 1e3}",
    // What the configuration's validation refuses.
    "\"env:\"",
    "\"${}\"",
    "a..b",
    "\"-x\"",
    "\"x(high)\"",
    "[\"env:\", \"${ }\", k]",
    "{levels: [low], min: 1e3, max: 0x10}",
    "[{name: a, alias: \"p q\"}, {name: b, alias: \"p q\"}, {name: c, alias: \"r(low)\"}, {name: d, alias: \"r(low)\"}, {name: e, thinking: {min: 9, max: 3}}]",
    "[{api-key: \"env:\"}, {api-key: k2, weight: 3}]",
];

/// A mapping of some of `fields`, each with a value from `value`.
fn random_mapping(
    rng: &mut Rng,
    fields: &[&str],
    value: &dyn Fn(&mut Rng, &str) -> String,
) -> String {
    let mut parts = Vec::new();
    for field in fields {
        if rng.below(3) > 0 {
            parts.push(format!("{field}: {}", value(rng, field)));
        }
    }
    format!("{{{}}}", parts.join(", "))
}

fn random_entry(rng: &mut Rng) -> String {
    const FIELDS: &[&str] = &[
        "name",
        "api-key",
        "base-url",
        "proxy-url",
        "prefix",
        "priority",
        "weight",
        "websockets",
        "disabled",
        "headers",
        "models",
        "excluded-models",
        "api-keys",
        "keys",
        "api-key-entries",
    ];
    random_mapping(rng, FIELDS, &|rng, field| match field {
        "keys" | "api-key-entries" if rng.below(4) > 0 => {
            const KEY_FIELDS: &[&str] = &["api-key", "weight", "proxy-url", "prefix", "priority"];
            let count = rng.below(3);
            let keys: Vec<String> = (0..count)
                .map(|_| {
                    random_mapping(rng, KEY_FIELDS, &|rng, _| rng.pick(ODD_SCALARS).to_string())
                })
                .collect();
            format!("[{}]", keys.join(", "))
        }
        "base-url" if rng.below(2) > 0 => "\"https://example.com/v1\"".to_string(),
        _ => rng.pick(ODD_SCALARS).to_string(),
    })
}

fn random_entries(rng: &mut Rng) -> String {
    if rng.below(8) == 0 {
        return rng.pick(ODD_SCALARS).to_string();
    }
    let count = rng.below(4);
    let entries: Vec<String> = (0..count).map(|_| random_entry(rng)).collect();
    format!("[{}]", entries.join(", "))
}

fn random_payload(rng: &mut Rng) -> String {
    const SECTIONS: &[&str] = &[
        "default",
        "default-raw",
        "override",
        "override-raw",
        "filter",
    ];
    random_mapping(rng, SECTIONS, &|rng, _| {
        let count = rng.below(3);
        let rules: Vec<String> = (0..count)
            .map(|_| {
                random_mapping(rng, &["models", "params"], &|rng, _| {
                    rng.pick(ODD_SCALARS).to_string()
                })
            })
            .collect();
        format!("[{}]", rules.join(", "))
    })
}

fn random_document(rng: &mut Rng) -> String {
    const SCALAR_KEYS: &[&str] = &[
        "host",
        "port",
        "debug",
        "logging-to-file",
        "logs-max-total-size-mb",
        "request-log",
        "usage-statistics-enabled",
        "proxy-url",
        "request-retry",
        "max-retry-interval",
        "disable-cooling",
        "transient-error-cooldown-seconds",
        "force-model-prefix",
        "passthrough-headers",
        "auth-dir",
    ];
    const SECTIONS: &[(&str, &[&str])] = &[
        (
            "remote-management",
            &["secret-key", "allow-remote", "disable-control-panel"],
        ),
        (
            "management",
            &["secret-key", "allow-remote", "disable-control-panel"],
        ),
        ("server", &["host", "port", "tls"]),
        ("tls", &["enable", "cert", "key"]),
        ("access", &["api-keys"]),
        (
            "routing",
            &[
                "strategy",
                "session-affinity",
                "session-affinity-ttl",
                "retry",
                "cooldown",
            ],
        ),
        ("streaming", &["keepalive-seconds", "bootstrap-retries"]),
        (
            "quota-exceeded",
            &["switch-project", "switch-preview-model"],
        ),
    ];
    const FAMILIES: &[&str] = &[
        "gemini-api-key",
        "claude-api-key",
        "codex-api-key",
        "vertex-api-key",
        "openai-compatibility",
    ];
    let scalar = |rng: &mut Rng, _: &str| rng.pick(ODD_SCALARS).to_string();
    let mut text = String::new();
    for key in SCALAR_KEYS {
        if rng.below(3) == 0 {
            text.push_str(&format!("{key}: {}\n", rng.pick(ODD_SCALARS)));
        }
    }
    for (section, fields) in SECTIONS {
        if rng.below(3) == 0 {
            text.push_str(&format!(
                "{section}: {}\n",
                random_mapping(rng, fields, &scalar)
            ));
        }
    }
    for family in FAMILIES {
        if rng.below(2) == 0 {
            text.push_str(&format!("{family}: {}\n", random_entries(rng)));
        }
    }
    // `api-keys` is the client key list in one layout and the provider
    // families in the other.
    match rng.below(4) {
        0 => text.push_str(&format!("api-keys: {}\n", rng.pick(ODD_SCALARS))),
        1 => {
            const NESTED: &[&str] = &[
                "gemini",
                "claude",
                "codex",
                "vertex",
                "openai-compatibility",
            ];
            let families = random_mapping(rng, NESTED, &|rng, _| random_entries(rng));
            text.push_str(&format!("api-keys: {families}\n"));
        }
        _ => {}
    }
    match rng.below(4) {
        0 => text.push_str(&format!("payload: {}\n", random_payload(rng))),
        1 => text.push_str(&format!("requests: {{payload: {}}}\n", random_payload(rng))),
        _ => {}
    }
    text
}

/// Whatever the values are — numbers where text belongs, text where a list
/// belongs, in any spelling — a document that is YAML converts into a
/// configuration that validates, or is refused as not being one.
#[test]
fn odd_values_in_known_settings_always_give_a_valid_file() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut converted = 0usize;
    for _ in 0..1000 {
        let text = random_document(&mut rng);
        match convert(&text, "config.yaml") {
            Ok(imported) => {
                converted += 1;
                assert!(imported.config.validate().is_empty(), "{text}");
            }
            Err(ConvertError::Yaml(_)) => {}
            Err(ConvertError::Invalid(message)) => panic!("{message}\n{text}"),
        }
    }
    // The generator writes YAML: nearly every document converts.
    assert!(converted > 900, "only {converted} documents converted");
}
