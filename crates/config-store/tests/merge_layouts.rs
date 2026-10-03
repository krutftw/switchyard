//! Format-preserving updates on less common file layouts: sub-tables written
//! apart from their parents, `[a.b]` sub-table style for maps, sections with
//! no header of their own, comments that open the file.

use pretty_assertions::assert_eq;
use switchyard_config_store::merge::{Strategy, render_update};
use switchyard_config_store::validate_text;
use switchyard_core::Config;
use switchyard_core::config::{
    ModelConfig, ProviderConfig, ProviderKind, Strategy as RoutingStrategy,
};

fn edit(text: &str, change: impl FnOnce(&mut Config)) -> String {
    let mut config = validate_text(text).expect("base text must be valid");
    change(&mut config);
    assert!(config.validate().is_empty());
    let rendered = render_update(text, &config).expect("render");
    assert_eq!(rendered.strategy, Strategy::Merged, "{}", rendered.text);
    let reparsed = validate_text(&rendered.text)
        .unwrap_or_else(|issues| panic!("{issues:?}\n---\n{}", rendered.text));
    assert_eq!(reparsed, config, "{}", rendered.text);
    rendered.text
}

fn model(id: &str) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        ..ModelConfig::default()
    }
}

/// Valid TOML, though nobody should write it: the sub-tables of provider
/// `a` come after an unrelated section.
const SCATTERED: &str = r#"[[providers]]
name = "a"
kind = "mock"

[server]
port = 9000

# a's models
[[providers.models]]
id = "m1"

[providers.headers]
X-A = "1"

[[providers]]
name = "b"
kind = "mock"
"#;

#[test]
fn scattered_sub_tables_stay_with_their_element() {
    let config = validate_text(SCATTERED).unwrap();
    assert_eq!(config.providers[0].models[0].id, "m1");
    assert_eq!(config.providers[0].headers["X-A"], "1");
    assert!(config.providers[1].models.is_empty());

    // A provider inserted after `a` must come after all of a's sub-tables,
    // or they would attach to the new provider.
    let out = edit(SCATTERED, |c| {
        c.providers
            .insert(1, ProviderConfig::new("c", ProviderKind::Mock));
    });
    assert_eq!(
        out,
        SCATTERED.replace(
            "X-A = \"1\"\n",
            "X-A = \"1\"\n\n[[providers]]\nname = \"c\"\nkind = \"mock\"\n"
        )
    );

    // A model added to `a` goes after its existing model.
    let out = edit(SCATTERED, |c| c.providers[0].models.push(model("m2")));
    assert_eq!(
        out,
        SCATTERED.replace(
            "id = \"m1\"\n",
            "id = \"m1\"\n\n[[providers.models]]\nid = \"m2\"\n"
        )
    );

    // A model added to `b` goes after `b`, not near a's models.
    let out = edit(SCATTERED, |c| c.providers[1].models.push(model("m3")));
    assert_eq!(
        out,
        format!("{SCATTERED}\n[[providers.models]]\nid = \"m3\"\n")
    );

    // Swapping the providers: the four tables of the two blocks exchange
    // their places, the unrelated section stays where it is.
    let out = edit(SCATTERED, |c| c.providers.swap(0, 1));
    assert_eq!(
        out,
        r#"[[providers]]
name = "b"
kind = "mock"

[server]
port = 9000

[[providers]]
name = "a"
kind = "mock"

# a's models
[[providers.models]]
id = "m1"

[providers.headers]
X-A = "1"
"#
    );

    // Removing `a` removes its scattered sub-tables too.
    let out = edit(SCATTERED, |c| {
        c.providers.remove(0);
    });
    assert_eq!(
        out,
        "[server]\nport = 9000\n\n[[providers]]\nname = \"b\"\nkind = \"mock\"\n"
    );
}

#[test]
fn maps_written_as_sub_tables_stay_sub_tables() {
    let text = r#"[[providers]]
name = "p"
kind = "mock"

[providers.headers]
X-A = "1"   # first
X-B = "2"

[[providers]]
name = "q"
kind = "mock"
"#;
    let out = edit(text, |c| {
        let headers = &mut c.providers[0].headers;
        headers.insert("X-C".into(), "3".into());
        headers.insert("X-A".into(), "one".into());
        headers.shift_remove("X-B");
    });
    assert_eq!(
        out,
        text.replace(
            "X-A = \"1\"   # first\nX-B = \"2\"\n",
            "X-A = \"one\"   # first\nX-C = \"3\"\n"
        )
    );

    // No headers left: the sub-table goes.
    let out = edit(text, |c| c.providers[0].headers.clear());
    assert_eq!(
        out,
        text.replace(
            "[providers.headers]\nX-A = \"1\"   # first\nX-B = \"2\"\n\n",
            ""
        )
    );

    // A provider without the map gets it inline.
    let out = edit(text, |c| {
        c.providers[1].headers.insert("X-Z".into(), "z".into());
    });
    assert_eq!(out, format!("{text}headers = {{ X-Z = \"z\" }}\n"));
}

#[test]
fn a_parent_header_is_added_above_its_existing_sub_section() {
    let text = "# cooldowns\n[routing.cooldown]\ntransient_secs = 5\n\n[server]\nport = 9000\n";
    let out = edit(text, |c| c.routing.strategy = RoutingStrategy::Weighted);
    assert_eq!(
        out,
        "[routing]\nstrategy = \"weighted\"\n\n# cooldowns\n[routing.cooldown]\ntransient_secs = 5\n\n[server]\nport = 9000\n"
    );

    // With a client key list but no [auth] header.
    let text = "[server]\nport = 9000\n\n[[auth.keys]]\nkey = \"sy-a\"\n";
    let out = edit(text, |c| c.auth.required = false);
    assert_eq!(
        out,
        "[server]\nport = 9000\n\n[auth]\nrequired = false\n\n[[auth.keys]]\nkey = \"sy-a\"\n"
    );
}

#[test]
fn the_comment_that_opens_the_file_survives_its_first_table() {
    let text = r#"# Switchyard configuration for the lab.
# Ask Sam before changing anything.

[[providers]]
name = "old"
kind = "mock"

[server]
port = 9000
"#;
    let out = edit(text, |c| c.providers.clear());
    assert_eq!(
        out,
        "# Switchyard configuration for the lab.\n# Ask Sam before changing anything.\n\n[server]\nport = 9000\n"
    );

    // …even when nothing else is left to carry it.
    let text = "# Lab gateway.\n\n# the only provider\n[[providers]]\nname = \"old\"\nkind = \"mock\"\n# bye\n";
    let out = edit(text, |c| c.providers.clear());
    assert_eq!(out, "# Lab gateway.\n\n# bye\n");

    // A table added in front of the first one goes below the comment that
    // opens the file…
    let text = "# Lab gateway.\n\n# the second\n[[providers]]\nname = \"b\"\nkind = \"mock\"\n";
    let out = edit(text, |c| {
        c.providers
            .insert(0, ProviderConfig::new("a", ProviderKind::Mock));
    });
    assert_eq!(
        out,
        "# Lab gateway.\n\n[[providers]]\nname = \"a\"\nkind = \"mock\"\n\n# the second\n[[providers]]\nname = \"b\"\nkind = \"mock\"\n"
    );
    // …and removing it again restores the file.
    let back = edit(&out, |c| {
        c.providers.remove(0);
    });
    assert_eq!(back, text);

    // A comment written directly above a table stays with that table.
    let text = "# Lab gateway.\n[[providers]]\nname = \"b\"\nkind = \"mock\"\n";
    let out = edit(text, |c| {
        c.providers
            .insert(0, ProviderConfig::new("a", ProviderKind::Mock));
    });
    assert_eq!(
        out,
        "[[providers]]\nname = \"a\"\nkind = \"mock\"\n\n# Lab gateway.\n[[providers]]\nname = \"b\"\nkind = \"mock\"\n"
    );
}

#[test]
fn keys_that_need_quoting_are_quoted() {
    // Header names are HTTP tokens, some of which a bare TOML key cannot
    // hold; the fields a payload rule sets may be named almost anything (no
    // spaces, no empty parts).
    let text = "[[providers]]\nname = \"p\"\nkind = \"mock\"\nheaders = { \"X~Y\" = \"1\" }\n\n\
                [[payload.override]]\nmodels = [\"*\"]\nset = { \"X~Y!\" = \"1\" }\n";
    let out = edit(text, |c| {
        let headers = &mut c.providers[0].headers;
        headers.insert("dotted.name".into(), "a \"quoted\" value".into());
        headers.insert("plain-name_1".into(), "back\\slash".into());
        let set = &mut c.payload.overrides[0].set;
        set.insert("dotted.name".into(), "a \"quoted\" value".into());
        set.insert("ключ".into(), "значение".into());
        set.insert("plain-name_1".into(), "back\\slash".into());
    });
    let config = validate_text(&out).unwrap();
    let headers = &config.providers[0].headers;
    assert_eq!(headers["dotted.name"], "a \"quoted\" value");
    assert_eq!(headers["plain-name_1"], "back\\slash");
    let set = &config.payload.overrides[0].set;
    assert_eq!(set["dotted.name"], "a \"quoted\" value");
    assert_eq!(set["ключ"], "значение");
    assert_eq!(set["plain-name_1"], "back\\slash");
    assert!(out.contains("\"X~Y\" = \"1\", \"dotted.name\" = "), "{out}");
    assert!(
        out.contains("\"X~Y!\" = \"1\", \"dotted.name\" = "),
        "{out}"
    );
    assert!(out.contains("plain-name_1 = "), "{out}");
}

#[test]
fn no_trailing_newline_and_blank_edges() {
    // The last line has no line break: the file gains one, nothing else.
    let text = "[server]\nport = 9000";
    let out = edit(text, |c| c.server.port = 9001);
    assert_eq!(out, "[server]\nport = 9001\n");

    // Leading blank lines and trailing whitespace are kept.
    let text = "\n\n[server]\nport = 9000\n\n\n";
    let out = edit(text, |c| c.server.port = 9001);
    assert_eq!(out, "\n\n[server]\nport = 9001\n\n\n");
}

#[test]
fn numbers_keep_their_spelling_unless_they_change() {
    let text = "[server]\nport = 9_000\nbody_limit_mb = 0x40\n\n[[pricing]]\nmodel = \"m\"\ninput = 1\noutput = 1e1\n";
    let config = validate_text(text).unwrap();
    assert_eq!(config.server.port, 9000);
    assert_eq!(config.server.body_limit_mb, 64);
    assert_eq!(config.pricing[0].input, 1.0);
    assert_eq!(config.pricing[0].output, 10.0);

    let out = edit(text, |c| c.server.cors = false);
    assert_eq!(out, text.replace("0x40\n", "0x40\ncors = false\n"));
    let out = edit(text, |c| c.pricing[0].output = 12.5);
    assert_eq!(out, text.replace("1e1", "12.5"));
}

#[test]
fn dotted_keys_inside_a_list_element() {
    let text = r#"[[providers]]
name = "p"
kind = "mock"
headers.X-A = "1"   # first
headers.X-B = "2"
prefix = "x"
"#;
    let out = edit(text, |c| {
        let headers = &mut c.providers[0].headers;
        headers.insert("X-A".into(), "one".into());
        headers.insert("X-C".into(), "3".into());
        c.providers[0].models.push(model("m1"));
    });
    assert_eq!(
        out,
        r#"[[providers]]
name = "p"
kind = "mock"
headers.X-A = "one"   # first
headers.X-B = "2"
headers.X-C = "3"
prefix = "x"

[[providers.models]]
id = "m1"
"#
    );

    let out = edit(text, |c| c.providers[0].headers.clear());
    assert_eq!(
        out,
        "[[providers]]\nname = \"p\"\nkind = \"mock\"\nprefix = \"x\"\n"
    );
}

#[test]
fn windows_line_endings_survive_structural_edits() {
    let unix = r#"# Lab gateway.

# first
[[providers]]
name = "a"
kind = "mock"
api_keys = [
    "k1", # one
    "k2",
]

# second
[[providers]]
name = "b"
kind = "mock"
"#;
    let windows = unix.replace('\n', "\r\n");
    let out = edit(&windows, |c| {
        c.providers.swap(0, 1);
        c.providers[1].api_keys.push("k3".into());
        c.providers
            .push(ProviderConfig::new("c", ProviderKind::Mock));
    });
    let expected = r#"# Lab gateway.

# second
[[providers]]
name = "b"
kind = "mock"

# first
[[providers]]
name = "a"
kind = "mock"
api_keys = [
    "k1", # one
    "k2",
    "k3",
]

[[providers]]
name = "c"
kind = "mock"
"#;
    assert_eq!(out, expected.replace('\n', "\r\n"));
    assert!(!out.replace("\r\n", "").contains(['\r', '\n']));

    // Removing the first provider keeps the comment that opens the file.
    let out = edit(&windows, |c| {
        c.providers.remove(0);
    });
    assert_eq!(
        out,
        "# Lab gateway.\n\n# second\n[[providers]]\nname = \"b\"\nkind = \"mock\"\n"
            .replace('\n', "\r\n")
    );
}

#[test]
fn awkward_strings_round_trip() {
    let awkward = [
        "line one\nline two",
        "tab\there",
        "quote \" and 'apostrophe'",
        "back\\slash",
        "bell\u{7}",
        "   ",
        "",
        "# not a comment",
        "\u{1F600} emoji",
        "'''",
        "\"\"\"",
        "trailing newline\n",
    ];
    // The values a payload rule sets and its model patterns take any text
    // (header values, alias targets and rule paths do not: no control
    // characters, no padding), so they carry the awkward values; a header
    // gets the ones it may hold.
    let text = "[[providers]]\nname = \"p\"\nkind = \"mock\"\nheaders = { First = 'literal' }\n\n\
                [[payload.default]]\nmodels = ['*']\nset = { First = 'literal' }\n\n\
                [[payload.filter]]\nmodels = ['*']\nremove = ['one']\n";
    for value in awkward {
        let as_header = !value.trim().is_empty() && !value.chars().any(char::is_control);
        // As a new key of an inline table, as a changed value of a literal
        // string, as a value of a block table, and as an array element.
        let out = edit(text, |c| {
            let p = &mut c.providers[0];
            if as_header {
                p.headers.insert("X-New".into(), value.into());
                p.headers.insert("First".into(), value.into());
            }
            p.prefix = format!("x{}", value.len());
            p.credentials
                .push(switchyard_core::config::CredentialConfig {
                    label: value.into(),
                    ..Default::default()
                });
            let set = &mut c.payload.default[0].set;
            set.insert("X-New".into(), value.into());
            set.insert("First".into(), value.into());
            c.payload.filter[0].models[0] = format!("{value}!");
            c.payload.filter[0].models.push(format!("{value}?"));
        });
        let config = validate_text(&out).unwrap();
        if as_header {
            assert_eq!(config.providers[0].headers["X-New"], value, "{out}");
            assert_eq!(config.providers[0].headers["First"], value, "{out}");
        }
        assert_eq!(config.payload.default[0].set["X-New"], value, "{out}");
        assert_eq!(config.payload.default[0].set["First"], value, "{out}");
        assert_eq!(config.providers[0].credentials[0].label, value, "{out}");
        assert_eq!(
            config.payload.filter[0].models,
            vec![format!("{value}!"), format!("{value}?")]
        );
    }
}

#[test]
fn line_breaks_inside_values_survive_windows_line_endings() {
    // (A header value may not hold a line break; what a payload rule sets
    // may.)
    let text = "# note\r\n[[payload.default]]\r\nmodels = [\"*\"]\r\nset = { a = 1 }\r\n\r\n\
                [[providers]]\r\nname = \"p\"\r\nkind = \"mock\"\r\n";
    let out = edit(text, |c| {
        c.payload.default[0]
            .set
            .insert("X-Multi".into(), "one\ntwo".into());
        c.providers[0]
            .credentials
            .push(switchyard_core::config::CredentialConfig {
                label: "first\nsecond\r\nthird".into(),
                ..Default::default()
            });
    });
    assert_eq!(
        out,
        "# note\r\n[[payload.default]]\r\nmodels = [\"*\"]\r\n\
         set = { a = 1, X-Multi = \"one\\ntwo\" }\r\n\r\n\
         [[providers]]\r\nname = \"p\"\r\nkind = \"mock\"\r\n\r\n\
         [[providers.credentials]]\r\nlabel = \"first\\nsecond\\r\\nthird\"\r\n"
    );
    let config = validate_text(&out).unwrap();
    assert_eq!(config.payload.default[0].set["X-Multi"], "one\ntwo");
    assert_eq!(
        config.providers[0].credentials[0].label,
        "first\nsecond\r\nthird"
    );
}
