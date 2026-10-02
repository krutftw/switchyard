//! The shipped `switchyard.example.toml` as the base document: a series of
//! dashboard-style edits must leave every one of its comments intact.

use pretty_assertions::assert_eq;
use switchyard_config_store::merge::{Strategy, render_update};
use switchyard_config_store::validate_text;
use switchyard_core::Config;
use switchyard_core::config::{
    AliasConfig, ClientKey, ModelConfig, PayloadRule, PriceConfig, ProviderConfig, ProviderKind,
    Strategy as RoutingStrategy, TlsConfig,
};

const EXAMPLE: &str = include_str!("../../../switchyard.example.toml");

/// The example may be checked out with either line ending.
fn example() -> String {
    EXAMPLE.replace("\r\n", "\n")
}

fn edit(text: &str, change: impl FnOnce(&mut Config)) -> String {
    let mut config = validate_text(text).expect("valid base");
    change(&mut config);
    assert!(config.validate().is_empty());
    let rendered = render_update(text, &config).expect("render");
    assert_eq!(rendered.strategy, Strategy::Merged);
    assert_eq!(validate_text(&rendered.text).expect("valid output"), config);
    rendered.text
}

/// Text of every comment in the file, in order: whole-line comments and the
/// comments that end a line.
fn comments(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                return Some(trimmed.to_string());
            }
            // The example never has `#` inside a value on a line that also
            // carries a comment, except in quoted strings handled here.
            let mut in_string = false;
            for (i, c) in line.char_indices() {
                match c {
                    '"' => in_string = !in_string,
                    '#' if !in_string => return Some(line[i..].to_string()),
                    _ => {}
                }
            }
            None
        })
        .collect()
}

#[test]
fn the_example_is_the_default_configuration() {
    let text = example();
    assert_eq!(validate_text(&text).unwrap(), Config::default());
    let rendered = render_update(&text, &Config::default()).unwrap();
    assert_eq!(rendered.strategy, Strategy::Unchanged);
    assert_eq!(rendered.text, text);
    assert!(comments(&text).len() > 100);
}

#[test]
fn a_series_of_edits_keeps_every_comment() {
    let original = example();
    let original_comments = comments(&original);
    let mut text = original.clone();
    let mut expected = original.clone();

    // 1. Set the admin secret: the line changes, the comment above stays.
    text = edit(&text, |c| c.admin.secret = "env:SWITCHYARD_ADMIN".into());
    expected = expected.replace("secret = \"\"\n", "secret = \"env:SWITCHYARD_ADMIN\"\n");
    assert_eq!(text, expected);

    // 2. Listen on all interfaces, another port: trailing comments stay.
    text = edit(&text, |c| {
        c.server.host = "0.0.0.0".into();
        c.server.port = 9000;
    });
    expected = expected
        .replace(
            "host = \"127.0.0.1\"        # \"0.0.0.0\" to accept",
            "host = \"0.0.0.0\"        # \"0.0.0.0\" to accept",
        )
        .replace("port = 8317\n", "port = 9000\n");
    assert_eq!(text, expected);

    // 3. The first client key: right under [auth], above the commented
    //    example.
    text = edit(&text, |c| {
        c.auth.keys.push(ClientKey {
            key: "sy-0123456789abcdef".into(),
            name: "laptop".into(),
            enabled: true,
            models: Vec::new(),
            rate_limit_rpm: None,
        });
    });
    expected = expected.replace(
        "required = true           # false: accept requests without a key (local use only)\n",
        "required = true           # false: accept requests without a key (local use only)\n\n\
         [[auth.keys]]\nkey = \"sy-0123456789abcdef\"\nname = \"laptop\"\n",
    );
    assert_eq!(text, expected);

    // 4. Routing settings, nested section included.
    text = edit(&text, |c| {
        c.routing.strategy = RoutingStrategy::FillFirst;
        c.routing.max_wait_secs = 30;
        c.routing.cooldown.rate_limit_max_secs = 600;
        c.routing.cooldown.enabled = false;
    });
    expected = expected
        .replace(
            "strategy = \"round-robin\"  # round-robin |",
            "strategy = \"fill-first\"  # round-robin |",
        )
        .replace(
            "max_wait_secs = 0         # wait",
            "max_wait_secs = 30         # wait",
        )
        .replace(
            "rate_limit_max_secs = 1800  # …up to",
            "rate_limit_max_secs = 600  # …up to",
        )
        .replace(
            "[routing.cooldown]\nenabled = true\n",
            "[routing.cooldown]\nenabled = false\n",
        );
    assert_eq!(text, expected);

    // 5. Two providers, one with models: appended after the last section,
    //    in front of the documentation that ends the file.
    text = edit(&text, |c| {
        let mut openai = ProviderConfig::new("openai", ProviderKind::Openai);
        openai.api_keys = vec!["env:OPENAI_API_KEY".into()];
        let mut local = ProviderConfig::new("ollama", ProviderKind::OpenaiCompat);
        local.base_url = "http://127.0.0.1:11434/v1".into();
        local.models = vec![ModelConfig {
            id: "llama3.3".into(),
            alias: "llama".into(),
            ..ModelConfig::default()
        }];
        c.providers = vec![openai, local];
    });
    expected = expected.replace(
        "retention_days = 30\n",
        "retention_days = 30\n\n\
         [[providers]]\nname = \"openai\"\nkind = \"openai\"\napi_keys = [\"env:OPENAI_API_KEY\"]\n\n\
         [[providers]]\nname = \"ollama\"\nkind = \"openai-compat\"\nbase_url = \"http://127.0.0.1:11434/v1\"\n\n\
         [[providers.models]]\nid = \"llama3.3\"\nalias = \"llama\"\n",
    );
    assert_eq!(text, expected);

    // 6. An alias, a price and a payload rule.
    text = edit(&text, |c| {
        c.aliases.push(AliasConfig {
            name: "smart".into(),
            targets: vec!["gpt-5(high)".into(), "llama".into()],
            hide_targets: false,
        });
        c.pricing.push(PriceConfig {
            model: "gpt-5*".into(),
            input: 1.25,
            output: 10.0,
            cache_read: Some(0.125),
            cache_write: None,
        });
        c.payload.filter.push(PayloadRule {
            models: vec!["*".into()],
            provider: "ollama".into(),
            remove: vec!["metadata".into(), "store".into()],
            ..PayloadRule::default()
        });
    });
    expected = expected.replace(
        "id = \"llama3.3\"\nalias = \"llama\"\n",
        "id = \"llama3.3\"\nalias = \"llama\"\n\n\
         [[aliases]]\nname = \"smart\"\ntargets = [\"gpt-5(high)\", \"llama\"]\n\n\
         [[payload.filter]]\nmodels = [\"*\"]\nprovider = \"ollama\"\nremove = [\"metadata\", \"store\"]\n\n\
         [[pricing]]\nmodel = \"gpt-5*\"\ninput = 1.25\noutput = 10.0\ncache_read = 0.125\n",
    );
    assert_eq!(text, expected);

    // 7. TLS: a sub-section right after [server].
    text = edit(&text, |c| {
        c.server.tls = Some(TlsConfig {
            cert: "cert.pem".into(),
            key: "key.pem".into(),
        });
    });
    expected = expected.replace(
        "data_dir = \"data\"         # usage history and request logs; relative to this file\n",
        "data_dir = \"data\"         # usage history and request logs; relative to this file\n\n\
         [server.tls]\ncert = \"cert.pem\"\nkey = \"key.pem\"\n",
    );
    assert_eq!(text, expected);

    // 8. Add a key to a provider and a second client key; reorder providers.
    text = edit(&text, |c| {
        c.providers[0].api_keys.push("env:OPENAI_API_KEY_2".into());
        c.providers[0].priority = 10;
        c.providers.swap(0, 1);
        c.auth.keys.insert(
            0,
            ClientKey {
                key: "sy-fedcba9876543210".into(),
                name: "ci".into(),
                enabled: true,
                models: vec!["llama".into()],
                rate_limit_rpm: Some(60),
            },
        );
    });
    expected = expected
        .replace(
            "[[providers]]\nname = \"openai\"\nkind = \"openai\"\napi_keys = [\"env:OPENAI_API_KEY\"]\n\n\
             [[providers]]\nname = \"ollama\"\nkind = \"openai-compat\"\nbase_url = \"http://127.0.0.1:11434/v1\"\n\n\
             [[providers.models]]\nid = \"llama3.3\"\nalias = \"llama\"\n",
            "[[providers]]\nname = \"ollama\"\nkind = \"openai-compat\"\nbase_url = \"http://127.0.0.1:11434/v1\"\n\n\
             [[providers.models]]\nid = \"llama3.3\"\nalias = \"llama\"\n\n\
             [[providers]]\nname = \"openai\"\nkind = \"openai\"\napi_keys = [\"env:OPENAI_API_KEY\", \"env:OPENAI_API_KEY_2\"]\npriority = 10\n",
        )
        .replace(
            "[[auth.keys]]\nkey = \"sy-0123456789abcdef\"",
            "[[auth.keys]]\nkey = \"sy-fedcba9876543210\"\nname = \"ci\"\nmodels = [\"llama\"]\nrate_limit_rpm = 60\n\n\
             [[auth.keys]]\nkey = \"sy-0123456789abcdef\"",
        );
    assert_eq!(text, expected);

    // Every comment of the shipped file is still there, in order, untouched
    // (three of them sit on lines whose value changed).
    let now = comments(&text);
    assert_eq!(now.len(), original_comments.len());
    for (after, before) in now.iter().zip(&original_comments) {
        assert_eq!(after, before);
    }

    // 9. Undo everything: back to the shipped file, byte for byte, except
    //    for values that are now written explicitly at their defaults.
    text = edit(&text, |c| *c = Config::default());
    assert_eq!(text, original);
}

#[test]
fn toggling_every_boolean_of_the_example_and_back() {
    let original = example();
    let flipped = edit(&original, |c| {
        c.server.cors = !c.server.cors;
        c.admin.enabled = !c.admin.enabled;
        c.admin.allow_remote = !c.admin.allow_remote;
        c.admin.ui = !c.admin.ui;
        c.auth.required = !c.auth.required;
        c.routing.session_affinity = !c.routing.session_affinity;
        c.routing.force_model_prefix = !c.routing.force_model_prefix;
        c.routing.cooldown.enabled = !c.routing.cooldown.enabled;
        c.upstream.passthrough_headers = !c.upstream.passthrough_headers;
        c.logging.file = !c.logging.file;
        c.usage.enabled = !c.usage.enabled;
        c.usage.persist = !c.usage.persist;
    });
    assert_eq!(comments(&flipped), comments(&original));
    assert_eq!(flipped.lines().count(), original.lines().count());
    let changed = flipped
        .lines()
        .zip(original.lines())
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(changed, 12);

    let back = edit(&flipped, |c| *c = Config::default());
    assert_eq!(back, original);
}
