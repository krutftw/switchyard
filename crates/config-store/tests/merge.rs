//! Format-preserving updates: each test checks both that the rewritten text
//! parses to the intended configuration and that everything the edit did not
//! concern is byte-identical (by comparing against literal expected text).

use indexmap_free::headers;
use pretty_assertions::assert_eq;
use switchyard_config_store::merge::{REWRITE_HEADER, Strategy, render_update};
use switchyard_config_store::validate_text;
use switchyard_core::Config;
use switchyard_core::config::{
    AliasConfig, ClientKey, CredentialConfig, ModelConfig, PayloadRule, PriceConfig,
    ProviderConfig, ProviderKind, RequestLogMode, Strategy as RoutingStrategy, TlsConfig,
};
use switchyard_core::reasoning::{Effort, ThinkingSupport};

/// Small helpers that keep the tests free of an `indexmap` dev-dependency.
mod indexmap_free {
    use switchyard_core::config::ProviderConfig;

    pub fn headers(provider: &mut ProviderConfig, pairs: &[(&str, &str)]) {
        provider.headers.clear();
        for (name, value) in pairs {
            provider.headers.insert(name.to_string(), value.to_string());
        }
    }
}

const BASE: &str = r#"# Gateway for the team.
# Managed by hand; the dashboard may edit it too.

[server]
host = "0.0.0.0"   # reachable from the LAN
port = 9000

[admin]
secret = "env:ADMIN_SECRET"

[auth]
required = true

# Laptop key
[[auth.keys]]
key = "sy-laptop"
name = "laptop"   # Jane's

[[auth.keys]]
key = "sy-ci"
name = "ci"
models = ["gpt-*"]

# ---------------------------------------------------------------
# Providers

# Main OpenAI account
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["env:OPENAI_API_KEY", 'sk-second']   # two keys

# Local models
[[providers]]
name = "local"
kind = 'openai-compat'
base_url = "http://127.0.0.1:11434/v1"
enabled = false   # machine is off

[[providers.models]]
id = "llama3.3"
alias = "llama"   # short name

[[providers.models]]
id = "qwen3"

[[providers]]
name = "anthropic"
kind = "anthropic"
api_keys = [
    "env:ANTHROPIC_API_KEY",   # primary
    "env:ANTHROPIC_API_KEY_2",
]

[[aliases]]
name = "smart"
targets = ["claude-opus-4-5", "gpt-5(high)"]

# trailing note
"#;

/// Applies `change` to the configuration in `text` and returns the rewritten
/// text, after checking that it was merged (not rewritten from scratch) and
/// parses to exactly the intended configuration.
fn edit(text: &str, change: impl FnOnce(&mut Config)) -> String {
    let mut config = validate_text(text).expect("base text must be valid");
    change(&mut config);
    let issues = config.validate();
    assert!(
        issues.is_empty(),
        "edit produced an invalid config: {issues:?}"
    );
    let rendered = render_update(text, &config).expect("render");
    assert_eq!(rendered.strategy, Strategy::Merged, "{}", rendered.text);
    let reparsed = validate_text(&rendered.text)
        .unwrap_or_else(|issues| panic!("{issues:?}\n---\n{}", rendered.text));
    assert_eq!(reparsed, config, "{}", rendered.text);
    rendered.text
}

fn provider(name: &str, kind: ProviderKind) -> ProviderConfig {
    ProviderConfig::new(name, kind)
}

#[test]
fn base_is_valid_and_a_no_op_keeps_every_byte() {
    let config = validate_text(BASE).unwrap();
    assert_eq!(config.providers.len(), 3);
    let rendered = render_update(BASE, &config).unwrap();
    assert_eq!(rendered.strategy, Strategy::Unchanged);
    assert_eq!(rendered.text, BASE);
}

#[test]
fn changing_a_scalar_keeps_its_comments() {
    let out = edit(BASE, |c| c.server.host = "127.0.0.1".into());
    assert_eq!(
        out,
        BASE.replace(
            "host = \"0.0.0.0\"   # reachable from the LAN",
            "host = \"127.0.0.1\"   # reachable from the LAN"
        )
    );

    let out = edit(BASE, |c| c.server.port = 9001);
    assert_eq!(out, BASE.replace("port = 9000", "port = 9001"));

    // A literal string stays a literal string.
    let out = edit(BASE, |c| c.providers[0].api_keys[1] = "sk-third".into());
    assert_eq!(out, BASE.replace("'sk-second'", "'sk-third'"));
    let out = edit(BASE, |c| c.providers[1].kind = ProviderKind::Openai);
    assert_eq!(
        out,
        BASE.replace("kind = 'openai-compat'", "kind = 'openai'")
    );
}

#[test]
fn adding_a_key_appends_it_to_its_table() {
    let out = edit(BASE, |c| c.server.cors = false);
    assert_eq!(
        out,
        BASE.replace("port = 9000\n", "port = 9000\ncors = false\n")
    );

    let out = edit(BASE, |c| c.providers[0].prefix = "oa".into());
    assert_eq!(
        out,
        BASE.replace(
            "'sk-second']   # two keys\n",
            "'sk-second']   # two keys\nprefix = \"oa\"\n"
        )
    );
}

#[test]
fn a_missing_section_is_created_with_only_what_changed() {
    let out = edit(BASE, |c| {
        c.logging.level = "debug".into();
        c.logging.request_log = RequestLogMode::Errors;
    });
    assert_eq!(
        out,
        BASE.replace(
            "\n# trailing note\n",
            "\n[logging]\nlevel = \"debug\"\nrequest_log = \"errors\"\n\n# trailing note\n"
        )
    );

    // A nested section: the parent header appears only if it has values.
    let out = edit(BASE, |c| c.routing.cooldown.transient_secs = 5);
    assert_eq!(
        out,
        BASE.replace(
            "\n# trailing note\n",
            "\n[routing.cooldown]\ntransient_secs = 5\n\n# trailing note\n"
        )
    );
    let out = edit(BASE, |c| {
        c.routing.strategy = RoutingStrategy::FillFirst;
        c.routing.cooldown.transient_secs = 5;
    });
    assert_eq!(
        out,
        BASE.replace(
            "\n# trailing note\n",
            "\n[routing]\nstrategy = \"fill-first\"\n\n[routing.cooldown]\ntransient_secs = 5\n\n# trailing note\n"
        )
    );

    // A sub-section of an existing section follows it.
    let out = edit(BASE, |c| {
        c.server.tls = Some(TlsConfig {
            cert: "cert.pem".into(),
            key: "key.pem".into(),
        });
    });
    assert_eq!(
        out,
        BASE.replace(
            "port = 9000\n",
            "port = 9000\n\n[server.tls]\ncert = \"cert.pem\"\nkey = \"key.pem\"\n"
        )
    );
}

#[test]
fn removing_a_key_takes_its_own_comments_only() {
    // `enabled = false` goes back to the default, which is not written.
    let out = edit(BASE, |c| c.providers[1].enabled = true);
    assert_eq!(
        out,
        BASE.replace("enabled = false   # machine is off\n", "")
    );

    let out = edit(BASE, |c| c.auth.keys[0].name.clear());
    assert_eq!(out, BASE.replace("name = \"laptop\"   # Jane's\n", ""));

    let out = edit(BASE, |c| c.providers[1].models[0].alias.clear());
    assert_eq!(out, BASE.replace("alias = \"llama\"   # short name\n", ""));
}

#[test]
fn toggling_booleans_back_to_defaults() {
    // Always-written field: the explicit key is updated in place…
    let off = edit(BASE, |c| c.auth.required = false);
    assert_eq!(off, BASE.replace("required = true", "required = false"));
    // …and updated again rather than removed.
    let on = edit(&off, |c| c.auth.required = true);
    assert_eq!(on, BASE);

    // Field the file left to its default: added, then set back.
    let off = edit(BASE, |c| c.admin.ui = false);
    assert_eq!(
        off,
        BASE.replace(
            "secret = \"env:ADMIN_SECRET\"\n",
            "secret = \"env:ADMIN_SECRET\"\nui = false\n"
        )
    );
    let on = edit(&off, |c| c.admin.ui = true);
    assert_eq!(
        on,
        BASE.replace(
            "secret = \"env:ADMIN_SECRET\"\n",
            "secret = \"env:ADMIN_SECRET\"\nui = true\n"
        )
    );

    // Field that is omitted at its default: the key comes and goes.
    let off = edit(BASE, |c| c.providers[0].discover = false);
    assert_eq!(
        off,
        BASE.replace(
            "'sk-second']   # two keys\n",
            "'sk-second']   # two keys\ndiscover = false\n"
        )
    );
    let on = edit(&off, |c| c.providers[0].discover = true);
    assert_eq!(on, BASE);
}

#[test]
fn adding_a_provider_leaves_the_others_alone() {
    let mut gemini = provider("gemini", ProviderKind::Gemini);
    gemini.api_keys = vec!["env:GEMINI_API_KEY".into()];
    gemini.priority = 5;

    // At the end.
    let out = edit(BASE, |c| c.providers.push(gemini.clone()));
    let block = "[[providers]]\nname = \"gemini\"\nkind = \"gemini\"\napi_keys = [\"env:GEMINI_API_KEY\"]\npriority = 5\n";
    assert_eq!(
        out,
        BASE.replace(
            "    \"env:ANTHROPIC_API_KEY_2\",\n]\n",
            &format!("    \"env:ANTHROPIC_API_KEY_2\",\n]\n\n{block}")
        )
    );

    // In the middle: after the block of the provider before it, sub-tables
    // included.
    let out = edit(BASE, |c| c.providers.insert(2, gemini.clone()));
    assert_eq!(
        out,
        BASE.replace("id = \"qwen3\"\n", &format!("id = \"qwen3\"\n\n{block}"))
    );

    // At the front: under the section banner, above the first provider and
    // the comment that is about that provider.
    let out = edit(BASE, |c| c.providers.insert(0, gemini.clone()));
    assert_eq!(
        out,
        BASE.replace(
            "# Providers\n\n# Main OpenAI account\n",
            &format!("# Providers\n\n{block}\n# Main OpenAI account\n")
        )
    );
}

#[test]
fn a_new_provider_with_everything() {
    let mut p = provider("everything", ProviderKind::OpenaiCompat);
    p.base_url = "https://example.com/v1".into();
    p.api_keys = vec!["sk-a".into(), "sk-b".into()];
    headers(
        &mut p,
        &[
            ("X-Title", "switchyard"),
            ("HTTP-Referer", "https://x.test"),
        ],
    );
    p.exclude = vec!["*-preview".into()];
    p.legacy_max_tokens = Some(false);
    p.credentials = vec![CredentialConfig {
        api_key: "sk-team".into(),
        label: "team account".into(),
        weight: Some(3),
        ..CredentialConfig::default()
    }];
    p.models = vec![
        ModelConfig {
            id: "big-model".into(),
            alias: "big".into(),
            context_window: Some(200_000),
            thinking: Some(ThinkingSupport::levels(&[Effort::Low, Effort::High])),
            ..ModelConfig::default()
        },
        ModelConfig {
            id: "small-model".into(),
            ..ModelConfig::default()
        },
    ];
    let out = edit(BASE, |c| c.providers.push(p));
    let block = r#"
[[providers]]
name = "everything"
kind = "openai-compat"
base_url = "https://example.com/v1"
api_keys = ["sk-a", "sk-b"]
headers = { X-Title = "switchyard", HTTP-Referer = "https://x.test" }
exclude = ["*-preview"]
legacy_max_tokens = false

[[providers.credentials]]
api_key = "sk-team"
label = "team account"
weight = 3

[[providers.models]]
id = "big-model"
alias = "big"
context_window = 200000
thinking = { min = 0, max = 0, zero_allowed = false, dynamic_allowed = false, levels = ["low", "high"] }

[[providers.models]]
id = "small-model"
"#;
    assert_eq!(
        out,
        BASE.replace(
            "    \"env:ANTHROPIC_API_KEY_2\",\n]\n",
            &format!("    \"env:ANTHROPIC_API_KEY_2\",\n]\n{block}")
        )
    );
}

#[test]
fn removing_a_provider_keeps_detached_comments() {
    // The middle one, with its sub-tables and the comment directly above it.
    let out = edit(BASE, |c| {
        c.providers.remove(1);
    });
    let removed = r#"# Local models
[[providers]]
name = "local"
kind = 'openai-compat'
base_url = "http://127.0.0.1:11434/v1"
enabled = false   # machine is off

[[providers.models]]
id = "llama3.3"
alias = "llama"   # short name

[[providers.models]]
id = "qwen3"

"#;
    assert_eq!(out, BASE.replace(removed, ""));

    // The first one: its own comment goes, the section banner stays.
    let out = edit(BASE, |c| {
        c.providers.remove(0);
    });
    let removed = r#"# Main OpenAI account
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["env:OPENAI_API_KEY", 'sk-second']   # two keys

"#;
    assert_eq!(out, BASE.replace(removed, ""));

    // The last one.
    let out = edit(BASE, |c| {
        c.providers.remove(2);
    });
    let removed = r#"[[providers]]
name = "anthropic"
kind = "anthropic"
api_keys = [
    "env:ANTHROPIC_API_KEY",   # primary
    "env:ANTHROPIC_API_KEY_2",
]

"#;
    assert_eq!(out, BASE.replace(removed, ""));

    // All of them: the banner still survives, in front of what follows.
    let out = edit(BASE, |c| c.providers.clear());
    let start = BASE.find("# Main OpenAI account").unwrap();
    let end = BASE.find("[[aliases]]").unwrap();
    assert_eq!(out, format!("{}{}", &BASE[..start], &BASE[end..]));
}

#[test]
fn reordering_providers_moves_whole_blocks() {
    let out = edit(BASE, |c| c.providers.swap(0, 2));
    let expected = r#"# Gateway for the team.
# Managed by hand; the dashboard may edit it too.

[server]
host = "0.0.0.0"   # reachable from the LAN
port = 9000

[admin]
secret = "env:ADMIN_SECRET"

[auth]
required = true

# Laptop key
[[auth.keys]]
key = "sy-laptop"
name = "laptop"   # Jane's

[[auth.keys]]
key = "sy-ci"
name = "ci"
models = ["gpt-*"]

# ---------------------------------------------------------------
# Providers

[[providers]]
name = "anthropic"
kind = "anthropic"
api_keys = [
    "env:ANTHROPIC_API_KEY",   # primary
    "env:ANTHROPIC_API_KEY_2",
]

# Local models
[[providers]]
name = "local"
kind = 'openai-compat'
base_url = "http://127.0.0.1:11434/v1"
enabled = false   # machine is off

[[providers.models]]
id = "llama3.3"
alias = "llama"   # short name

[[providers.models]]
id = "qwen3"

# Main OpenAI account
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["env:OPENAI_API_KEY", 'sk-second']   # two keys

[[aliases]]
name = "smart"
targets = ["claude-opus-4-5", "gpt-5(high)"]

# trailing note
"#;
    assert_eq!(out, expected);

    // Moving the provider with sub-tables to the end keeps them with it.
    let out = edit(BASE, |c| {
        let local = c.providers.remove(1);
        c.providers.push(local);
    });
    let local_block = r#"# Local models
[[providers]]
name = "local"
kind = 'openai-compat'
base_url = "http://127.0.0.1:11434/v1"
enabled = false   # machine is off

[[providers.models]]
id = "llama3.3"
alias = "llama"   # short name

[[providers.models]]
id = "qwen3"

"#;
    let anthropic_block = r#"[[providers]]
name = "anthropic"
kind = "anthropic"
api_keys = [
    "env:ANTHROPIC_API_KEY",   # primary
    "env:ANTHROPIC_API_KEY_2",
]

"#;
    assert_eq!(
        out,
        BASE.replace(
            &format!("{local_block}{anthropic_block}"),
            &format!("{anthropic_block}{local_block}")
        )
    );
}

#[test]
fn reordering_and_editing_nested_models() {
    // Swap the two models of `local` and edit one of them.
    let out = edit(BASE, |c| {
        c.providers[1].models.swap(0, 1);
        c.providers[1].models[1].display_name = "Llama 3.3".into();
    });
    assert_eq!(
        out,
        BASE.replace(
            "[[providers.models]]\nid = \"llama3.3\"\nalias = \"llama\"   # short name\n\n[[providers.models]]\nid = \"qwen3\"\n",
            "[[providers.models]]\nid = \"qwen3\"\n\n[[providers.models]]\nid = \"llama3.3\"\nalias = \"llama\"   # short name\ndisplay_name = \"Llama 3.3\"\n"
        )
    );

    // Edit a nested entry in place.
    let out = edit(BASE, |c| {
        c.providers[1].models[1].context_window = Some(32_768);
        c.providers[1].models[0].alias = "llama3".into();
    });
    assert_eq!(
        out,
        BASE.replace(
            "alias = \"llama\"   # short name",
            "alias = \"llama3\"   # short name"
        )
        .replace(
            "id = \"qwen3\"\n",
            "id = \"qwen3\"\ncontext_window = 32768\n"
        )
    );

    // Add a model to a provider that had none, and one to a provider in the
    // middle of the file.
    let out = edit(BASE, |c| {
        c.providers[0].models.push(ModelConfig {
            id: "gpt-5".into(),
            ..ModelConfig::default()
        });
        c.providers[1].models.push(ModelConfig {
            id: "phi4".into(),
            alias: "phi".into(),
            ..ModelConfig::default()
        });
    });
    assert_eq!(
        out,
        BASE.replace(
            "'sk-second']   # two keys\n",
            "'sk-second']   # two keys\n\n[[providers.models]]\nid = \"gpt-5\"\n"
        )
        .replace(
            "id = \"qwen3\"\n",
            "id = \"qwen3\"\n\n[[providers.models]]\nid = \"phi4\"\nalias = \"phi\"\n"
        )
    );

    // Remove the first model: the second keeps its text.
    let out = edit(BASE, |c| {
        c.providers[1].models.remove(0);
    });
    assert_eq!(
        out,
        BASE.replace(
            "[[providers.models]]\nid = \"llama3.3\"\nalias = \"llama\"   # short name\n\n",
            ""
        )
    );
}

#[test]
fn renaming_a_provider_edits_it_in_place() {
    let out = edit(BASE, |c| c.providers[1].name = "ollama".into());
    assert_eq!(out, BASE.replace("name = \"local\"", "name = \"ollama\""));
}

#[test]
fn first_client_key_is_added_and_last_one_removed() {
    let none = r#"# header

[auth]
required = true   # keep it on

# Routing
[routing]
strategy = "fill-first"
"#;
    let one = edit(none, |c| {
        c.auth.keys.push(ClientKey {
            key: "sy-first".into(),
            name: "first".into(),
            enabled: true,
            models: vec!["gpt-*".into(), "claude-*".into()],
            rate_limit_rpm: Some(120),
        });
    });
    assert_eq!(
        one,
        r#"# header

[auth]
required = true   # keep it on

[[auth.keys]]
key = "sy-first"
name = "first"
models = ["gpt-*", "claude-*"]
rate_limit_rpm = 120

# Routing
[routing]
strategy = "fill-first"
"#
    );

    let two = edit(&one, |c| {
        c.auth.keys.push(ClientKey {
            key: "sy-second".into(),
            name: String::new(),
            enabled: false,
            models: Vec::new(),
            rate_limit_rpm: None,
        });
    });
    assert_eq!(
        two,
        one.replace(
            "rate_limit_rpm = 120\n",
            "rate_limit_rpm = 120\n\n[[auth.keys]]\nkey = \"sy-second\"\nenabled = false\n"
        )
    );

    // Removing them again, last one included, restores the original bytes.
    let back_to_one = edit(&two, |c| {
        c.auth.keys.pop();
    });
    assert_eq!(back_to_one, one);
    let back_to_none = edit(&one, |c| c.auth.keys.clear());
    assert_eq!(back_to_none, none);

    // A file with no [auth] section at all.
    let bare = "[server]\nport = 9000\n";
    let out = edit(bare, |c| {
        c.auth.keys.push(ClientKey {
            key: "sy-only".into(),
            name: String::new(),
            enabled: true,
            models: Vec::new(),
            rate_limit_rpm: None,
        });
    });
    assert_eq!(
        out,
        "[server]\nport = 9000\n\n[[auth.keys]]\nkey = \"sy-only\"\n"
    );
    let out = edit(&out, |c| c.auth.keys.clear());
    assert_eq!(out, bare);
}

#[test]
fn client_key_rotation_and_removal_by_identity() {
    // Remove the first key: the second is untouched, the first one's comment
    // (directly above it) goes with it.
    let out = edit(BASE, |c| {
        c.auth.keys.remove(0);
    });
    assert_eq!(
        out,
        BASE.replace(
            "# Laptop key\n[[auth.keys]]\nkey = \"sy-laptop\"\nname = \"laptop\"   # Jane's\n\n",
            ""
        )
    );

    // Rotate a key: same entry, new secret.
    let out = edit(BASE, |c| c.auth.keys[0].key = "sy-laptop-2".into());
    assert_eq!(
        out,
        BASE.replace("key = \"sy-laptop\"", "key = \"sy-laptop-2\"")
    );
}

#[test]
fn plain_arrays_are_edited_element_wise_at_the_end() {
    // Single line: append and truncate.
    let out = edit(BASE, |c| c.auth.keys[1].models.push("claude-*".into()));
    assert_eq!(
        out,
        BASE.replace("models = [\"gpt-*\"]", "models = [\"gpt-*\", \"claude-*\"]")
    );
    let out = edit(BASE, |c| c.providers[0].api_keys.truncate(1));
    assert_eq!(
        out,
        BASE.replace(
            "[\"env:OPENAI_API_KEY\", 'sk-second']",
            "[\"env:OPENAI_API_KEY\"]"
        )
    );

    // Multi-line with a trailing comma and a comment.
    let out = edit(BASE, |c| {
        c.providers[2]
            .api_keys
            .push("env:ANTHROPIC_API_KEY_3".into());
    });
    assert_eq!(
        out,
        BASE.replace(
            "    \"env:ANTHROPIC_API_KEY_2\",\n]",
            "    \"env:ANTHROPIC_API_KEY_2\",\n    \"env:ANTHROPIC_API_KEY_3\",\n]"
        )
    );
    let out = edit(BASE, |c| {
        c.providers[2].api_keys.pop();
    });
    assert_eq!(
        out,
        BASE.replace("    \"env:ANTHROPIC_API_KEY_2\",\n]", "]")
    );

    // Anything else rewrites the array in its original shape.
    let out = edit(BASE, |c| c.providers[2].api_keys.swap(0, 1));
    assert_eq!(
        out,
        BASE.replace(
            "    \"env:ANTHROPIC_API_KEY\",   # primary\n    \"env:ANTHROPIC_API_KEY_2\",\n]",
            "    \"env:ANTHROPIC_API_KEY_2\",\n    \"env:ANTHROPIC_API_KEY\",\n]"
        )
    );
    let out = edit(BASE, |c| {
        c.aliases[0].targets.remove(0);
    });
    assert_eq!(
        out,
        BASE.replace(
            "targets = [\"claude-opus-4-5\", \"gpt-5(high)\"]",
            "targets = [\"gpt-5(high)\"]"
        )
    );
}

#[test]
fn multi_line_arrays_without_trailing_comma() {
    let text =
        "[[aliases]]\nname = \"a\"\ntargets = [\n  \"one\", # first\n  \"two\" # second\n]\n";
    let out = edit(text, |c| c.aliases[0].targets.push("three".into()));
    assert_eq!(
        out,
        "[[aliases]]\nname = \"a\"\ntargets = [\n  \"one\", # first\n  \"two\", # second\n  \"three\"\n]\n"
    );
    let out = edit(text, |c| {
        c.aliases[0].targets.pop();
    });
    assert_eq!(
        out,
        "[[aliases]]\nname = \"a\"\ntargets = [\n  \"one\" # first\n]\n"
    );
    let out = edit(text, |c| c.aliases[0].targets.reverse());
    assert_eq!(
        out,
        "[[aliases]]\nname = \"a\"\ntargets = [\n  \"two\",\n  \"one\"\n]\n"
    );

    // Spaces inside the brackets are kept.
    let text = "[[aliases]]\nname = \"a\"\ntargets = [ \"one\", \"two\" ]\n";
    let out = edit(text, |c| c.aliases[0].targets.push("three".into()));
    assert_eq!(
        out,
        "[[aliases]]\nname = \"a\"\ntargets = [ \"one\", \"two\", \"three\" ]\n"
    );
    let out = edit(text, |c| {
        c.aliases[0].targets.pop();
    });
    assert_eq!(out, "[[aliases]]\nname = \"a\"\ntargets = [ \"one\" ]\n");
}

#[test]
fn inline_styles_are_kept() {
    let text = r#"auth = { required = false, keys = [{ key = "sy-a", name = "a" }, { key = "sy-b" }] }
providers = [
    { name = "one", kind = "mock" },   # first
    { name = "two", kind = "mock", headers = { X-A = "1" } },
]

[server]
tls = { cert = "c.pem", key = "k.pem" }  # inline
"#;
    // Edit inside inline tables.
    let out = edit(text, |c| {
        c.server.tls.as_mut().unwrap().cert = "new.pem".into();
        c.auth.keys[0].name = "alpha".into();
        c.providers[1].prefix = "p2".into();
    });
    assert_eq!(
        out,
        text.replace("cert = \"c.pem\"", "cert = \"new.pem\"")
            .replace("name = \"a\"", "name = \"alpha\"")
            .replace(
                "headers = { X-A = \"1\" } }",
                "headers = { X-A = \"1\" }, prefix = \"p2\" }"
            )
    );

    // Add and remove keys of inline tables.
    let out = edit(text, |c| {
        headers(&mut c.providers[1], &[("X-A", "1"), ("X-B", "2")]);
        c.auth.keys[0].name.clear();
    });
    assert_eq!(
        out,
        text.replace("{ X-A = \"1\" }", "{ X-A = \"1\", X-B = \"2\" }")
            .replace("{ key = \"sy-a\", name = \"a\" }", "{ key = \"sy-a\" }")
    );

    // Append to the inline lists: they stay inline.
    let out = edit(text, |c| {
        c.providers.push(provider("three", ProviderKind::Mock));
        c.auth.keys.push(ClientKey {
            key: "sy-c".into(),
            name: String::new(),
            enabled: true,
            models: Vec::new(),
            rate_limit_rpm: None,
        });
    });
    assert_eq!(
        out,
        r#"auth = { required = false, keys = [{ key = "sy-a", name = "a" }, { key = "sy-b" }, { key = "sy-c" }] }
providers = [
    { name = "one", kind = "mock" },   # first
    { name = "two", kind = "mock", headers = { X-A = "1" } },
    { name = "three", kind = "mock" },
]

[server]
tls = { cert = "c.pem", key = "k.pem" }  # inline
"#
    );

    // Remove from the inline list by identity.
    let out = edit(text, |c| {
        c.providers.remove(0);
    });
    assert_eq!(
        out,
        r#"auth = { required = false, keys = [{ key = "sy-a", name = "a" }, { key = "sy-b" }] }
providers = [
    { name = "two", kind = "mock", headers = { X-A = "1" } },
]

[server]
tls = { cert = "c.pem", key = "k.pem" }  # inline
"#
    );

    // Removing the inline table altogether.
    let out = edit(text, |c| c.server.tls = None);
    assert_eq!(
        out,
        text.replace(
            "tls = { cert = \"c.pem\", key = \"k.pem\" }  # inline\n",
            ""
        )
    );
}

#[test]
fn dotted_keys_are_kept() {
    let text = "server.port = 9000\nserver.host = \"0.0.0.0\" # lan\n\n[admin]\nsecret = \"s\"\n";
    let out = edit(text, |c| c.server.port = 9001);
    assert_eq!(out, text.replace("9000", "9001"));
    let out = edit(text, |c| c.server.cors = false);
    assert_eq!(out, text.replace("# lan\n", "# lan\nserver.cors = false\n"));
    let out = edit(text, |c| {
        c.server.tls = Some(TlsConfig {
            cert: "c".into(),
            key: "k".into(),
        });
    });
    assert_eq!(
        out,
        text.replace(
            "# lan\n",
            "# lan\nserver.tls = { cert = \"c\", key = \"k\" }\n"
        )
    );
}

#[test]
fn payload_rules_and_pricing() {
    let text = r#"[[payload.default]]         # set only if the client did not
models = ["gemini-*"]
set = { "generationConfig.thinkingConfig.includeThoughts" = true }

[[payload.override]]        # always set
models = ["gpt-*"]
protocol = "openai-responses"
set = { "reasoning.summary" = "auto" }

[[payload.filter]]          # remove
models = ["*"]
provider = "ollama"
remove = ["metadata", "store"]

[[pricing]]
model = "gpt-5*"
input = 1.25
output = 10.0
"#;
    let out = edit(text, |c| {
        c.payload.overrides[0]
            .set
            .insert("store".into(), serde_json::json!(false));
        c.payload.filter[0].remove.push("user".into());
        c.pricing[0].cache_read = Some(0.125);
        c.pricing[0].output = 12.5;
    });
    assert_eq!(
        out,
        text.replace(
            "set = { \"reasoning.summary\" = \"auto\" }",
            "set = { \"reasoning.summary\" = \"auto\", store = false }"
        )
        .replace(
            "[\"metadata\", \"store\"]",
            "[\"metadata\", \"store\", \"user\"]"
        )
        .replace("output = 10.0\n", "output = 12.5\ncache_read = 0.125\n")
    );

    // New rules and prices; rules have no identity, so they match by content.
    let out = edit(text, |c| {
        c.payload.overrides.insert(
            0,
            PayloadRule {
                models: vec!["claude-*".into()],
                set: [(
                    "thinking".to_string(),
                    serde_json::json!({"type": "enabled", "budget_tokens": 2048}),
                )]
                .into_iter()
                .collect(),
                ..PayloadRule::default()
            },
        );
        c.pricing.push(PriceConfig {
            model: "claude-*".into(),
            input: 3.0,
            output: 15.0,
            cache_read: None,
            cache_write: None,
        });
    });
    assert_eq!(
        out,
        text.replace(
            "[[payload.override]]        # always set",
            "[[payload.override]]\nmodels = [\"claude-*\"]\nset = { thinking = { type = \"enabled\", budget_tokens = 2048 } }\n\n[[payload.override]]        # always set"
        ) + "\n[[pricing]]\nmodel = \"claude-*\"\ninput = 3.0\noutput = 15.0\n"
    );

    // Dropping the whole payload section.
    let out = edit(text, |c| {
        c.payload.default.clear();
        c.payload.overrides.clear();
        c.payload.filter.clear();
    });
    assert_eq!(
        out,
        "[[pricing]]\nmodel = \"gpt-5*\"\ninput = 1.25\noutput = 10.0\n"
    );
}

#[test]
fn aliases_are_matched_by_name() {
    let text = r#"# first alias
[[aliases]]
name = "smart"
targets = ["a", "b"]

# second alias
[[aliases]]
name = "fast"
targets = ["c"]
hide_targets = true
"#;
    let out = edit(text, |c| {
        c.aliases.swap(0, 1);
        c.aliases.push(AliasConfig {
            name: "cheap".into(),
            targets: vec!["d".into()],
            hide_targets: false,
        });
    });
    assert_eq!(
        out,
        r#"# second alias
[[aliases]]
name = "fast"
targets = ["c"]
hide_targets = true

# first alias
[[aliases]]
name = "smart"
targets = ["a", "b"]

[[aliases]]
name = "cheap"
targets = ["d"]
"#
    );
}

#[test]
fn windows_line_endings_and_byte_order_mark_survive() {
    let unix = "# note\n[server]\nport = 9000 # custom\n\n[admin]\nui = false\n";
    let windows = format!("\u{feff}{}", unix.replace('\n', "\r\n"));
    let out = edit(&windows, |c| {
        c.server.port = 9001;
        c.server.cors = false;
    });
    assert_eq!(
        out,
        format!(
            "\u{feff}{}",
            "# note\n[server]\nport = 9001 # custom\ncors = false\n\n[admin]\nui = false\n"
                .replace('\n', "\r\n")
        )
    );
}

#[test]
fn unreadable_text_is_rewritten_under_a_header() {
    let mut config = Config::default();
    config.server.port = 9000;
    config.providers.push(provider("mock", ProviderKind::Mock));

    for broken in ["[server\nport = ", "[server]\nprot = 1\n", "port = \"x\"\n"] {
        let rendered = render_update(broken, &config).unwrap();
        assert_eq!(rendered.strategy, Strategy::Rewritten);
        assert!(rendered.text.starts_with(REWRITE_HEADER));
        assert_eq!(validate_text(&rendered.text).unwrap(), config);
    }

    // An empty file is simply filled in, with only what differs from the
    // defaults.
    let rendered = render_update("", &config).unwrap();
    assert_eq!(rendered.strategy, Strategy::Merged);
    assert_eq!(
        rendered.text,
        "[server]\nport = 9000\n\n[[providers]]\nname = \"mock\"\nkind = \"mock\"\n"
    );
}

#[test]
fn values_toml_cannot_hold_are_refused() {
    let mut config = validate_text(BASE).unwrap();
    config.payload.overrides.push(PayloadRule {
        models: vec!["*".into()],
        set: [("user".to_string(), serde_json::Value::Null)]
            .into_iter()
            .collect(),
        ..PayloadRule::default()
    });
    let err = render_update(BASE, &config).unwrap_err();
    assert!(err.contains("payload.override.set.user"), "{err}");
    assert!(err.contains("null"), "{err}");

    let mut config = validate_text(BASE).unwrap();
    config.providers[1].models[0].context_window = Some(u64::MAX);
    let err = render_update(BASE, &config).unwrap_err();
    assert!(err.contains("providers.models.context_window"), "{err}");
}

#[test]
fn semantically_invalid_text_is_still_merged() {
    // Port 0 fails validation but the file is readable, so an edit that
    // fixes it keeps the formatting.
    let text = "# c\n[server]\nport = 0 # oops\n";
    let mut config: Config = toml::from_str(text).unwrap();
    config.server.port = 8000;
    let rendered = render_update(text, &config).unwrap();
    assert_eq!(rendered.strategy, Strategy::Merged);
    assert_eq!(rendered.text, "# c\n[server]\nport = 8000 # oops\n");
}

#[test]
fn detached_comment_of_a_removed_key_moves_to_the_next_key() {
    let text = r#"[[providers]]
name = "p"
kind = "mock"

# Routing knobs for this provider:

# higher wins
priority = 5
prefix = "x"
"#;
    let out = edit(text, |c| c.providers[0].priority = 0);
    assert_eq!(
        out,
        r#"[[providers]]
name = "p"
kind = "mock"

# Routing knobs for this provider:

prefix = "x"
"#
    );

    // Last key of the table: the paragraph ends the table's body.
    let text = "[[providers]]\nname = \"p\"\nkind = \"mock\"\n\n# About priorities.\n\npriority = 5\n\n[server]\nport = 9000\n";
    let out = edit(text, |c| c.providers[0].priority = 0);
    assert_eq!(
        out,
        "[[providers]]\nname = \"p\"\nkind = \"mock\"\n\n# About priorities.\n\n[server]\nport = 9000\n"
    );
}
