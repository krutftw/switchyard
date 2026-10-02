//! Review finding: removing the last payload rule deletes comments that are
//! not about it — the section banner and every other detached comment
//! paragraph above the rule.
//!
//! The brief: "a removed key disappears with its own same-line comment but
//! not the comments of neighbours", and for the shipped example file "its
//! comments must survive intact". The merge module's own rule (see its docs):
//! "Comment paragraphs set apart by a blank line — a section banner, the text
//! that opens the file, a commented-out example — belong to the *place*: they
//! stay where they are when the table below them is removed".
//!
//! That rule is honoured for `[[providers]]`, `[[aliases]]`, `[[pricing]]`,
//! `[[auth.keys]]`, and for one payload list while another is left. It is not
//! honoured when `payload` empties altogether — which is what removing the
//! *only* rule does, since an empty `[payload]` is not serialised. The key
//! `payload` then disappears from the value tree, `remove_key` finds an
//! (implicit) `Item::Table` holding the `[[payload.*]]` lists, and
//! `note_removed_item` looks at that table's own decoration only: it never
//! visits the lists inside, so the detached comments above their headers are
//! dropped with them.
//!
//! In a file laid out like the shipped example this is costly. Everything
//! between the last section and the first `[[payload.…]]` header is the
//! decoration of that header, so deleting one payload rule in the dashboard
//! deletes the provider, alias and payload documentation above it.
//!
//! The same blind spot shows wherever what is removed is a *table that holds
//! other things* rather than a value or a list of tables:
//!
//! * a map written with dotted keys (`headers.X-A = "1"`,
//!   `server.tls.cert = …`) is stored as a table without a position, for
//!   which `note_removed_table` returns at once — the detached comment above
//!   its first key is dropped, although the very same file with the map
//!   written inline (`headers = { … }`) keeps it (last two tests);
//! * the sub-tables of a removed list element (`[[providers.models]]`,
//!   `[[providers.credentials]]`, `[providers.headers]`,
//!   `[providers.models.thinking]`) are not visited either. Those comments sit
//!   inside the removed block, so that case is not asserted here.

use pretty_assertions::assert_eq;
use switchyard_config_store::merge::{Strategy, render_update};
use switchyard_config_store::validate_text;
use switchyard_core::Config;

const EXAMPLE: &str = include_str!("../../../switchyard.example.toml");

fn edit(text: &str, change: impl FnOnce(&mut Config)) -> String {
    let mut config = validate_text(text).expect("valid base");
    change(&mut config);
    assert!(config.validate().is_empty(), "{:?}", config.validate());
    let rendered = render_update(text, &config).expect("render");
    assert_eq!(rendered.strategy, Strategy::Merged);
    assert_eq!(validate_text(&rendered.text).expect("valid output"), config);
    rendered.text
}

const SECTIONS: &str = r#"[server]
port = 9000

# ---------------------------------------------------------------------------
# Payload rules patch the JSON sent upstream. Paths are dotted ("a.b.0.c").

# Reasoning summaries for every GPT model.
[[payload.override]]        # always set
models = ["gpt-*"]
set = { "reasoning.summary" = "auto" }

# ---------------------------------------------------------------------------
# Prices in USD per million tokens.

[[pricing]]
model = "gpt-5*"
input = 1.25
output = 10.0
"#;

/// The only payload rule is removed. Its own comments go with it (the one on
/// the header's line and the one directly above); the section banner, set
/// apart by a blank line, stays — exactly as it does for the last
/// `[[pricing]]` entry below.
#[test]
fn removing_the_only_payload_rule_keeps_the_section_banner() {
    let out = edit(SECTIONS, |c| c.payload.overrides.clear());
    let removed = r#"# Reasoning summaries for every GPT model.
[[payload.override]]        # always set
models = ["gpt-*"]
set = { "reasoning.summary" = "auto" }

"#;
    assert_eq!(out, SECTIONS.replace(removed, ""));
}

/// The control: the same edit on the neighbouring section already behaves
/// that way (this test passes).
#[test]
fn removing_the_only_price_keeps_the_section_banner() {
    let out = edit(SECTIONS, |c| c.pricing.clear());
    assert_eq!(
        out,
        SECTIONS.replace(
            "[[pricing]]\nmodel = \"gpt-5*\"\ninput = 1.25\noutput = 10.0\n",
            ""
        )
    );
}

/// Several lists, all emptied in one edit (`PUT /payload` with an empty
/// body): every comment paragraph that stands apart survives.
#[test]
fn emptying_every_payload_list_keeps_the_comments_that_stand_apart() {
    let text = r#"[server]
port = 9000

# ---------------------------------------------------------------------------
# Payload rules patch the JSON sent upstream.

[[payload.default]]
models = ["gemini-*"]
set = { "generationConfig.thinkingConfig.includeThoughts" = true }

# Keep for later:
# [[payload.override]]
# models = ["gpt-*"]

[[payload.filter]]
models = ["*"]
remove = ["metadata"]

# ---------------------------------------------------------------------------
# Usage.

[usage]
enabled = false
"#;
    let out = edit(text, |c| c.payload = Default::default());
    for kept in [
        "# Payload rules patch the JSON sent upstream.",
        "# Keep for later:",
        "# [[payload.override]]",
        "# models = [\"gpt-*\"]",
        "# Usage.",
    ] {
        assert!(
            out.lines().any(|line| line == kept),
            "the comment line {kept:?} was deleted:\n{out}"
        );
    }
}

/// The shipped example. A user switches the commented-out
/// `[[payload.override]]` example on by removing the `# ` in front of its
/// four lines, and later deletes that rule in the dashboard. The file must be
/// what it was, minus those four lines: the documentation above the rule (how
/// to configure providers, aliases and payload rules — about 90 comment
/// lines) is not the rule's to take.
#[test]
fn deleting_the_one_payload_rule_of_the_example_keeps_its_documentation() {
    let example = EXAMPLE.replace("\r\n", "\n");
    let commented = "# [[payload.override]]        # always set\n\
                     # models = [\"gpt-*\"]\n\
                     # protocol = \"openai-responses\"\n\
                     # set = { \"reasoning.summary\" = \"auto\" }\n";
    let enabled = "[[payload.override]]        # always set\n\
                   models = [\"gpt-*\"]\n\
                   protocol = \"openai-responses\"\n\
                   set = { \"reasoning.summary\" = \"auto\" }\n";
    assert!(example.contains(commented), "the example changed");
    let text = example.replace(commented, enabled);
    assert_eq!(validate_text(&text).unwrap().payload.overrides.len(), 1);

    let out = edit(&text, |c| c.payload.overrides.clear());

    let comment_lines = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|line| line.trim_start().starts_with('#'))
            .map(str::to_string)
            .collect()
    };
    let before = comment_lines(&text);
    let after = comment_lines(&out);
    let lost: Vec<&String> = before.iter().filter(|line| !after.contains(line)).collect();
    assert!(
        lost.is_empty(),
        "{} of {} comment lines were deleted together with one payload rule, starting with:\n{}",
        lost.len(),
        before.len(),
        lost.iter()
            .take(8)
            .map(|line| line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The control for the dotted-key tests below: a map written inline. The comment
/// paragraph above it stands apart (blank lines on both sides) and stays when
/// the map is removed. This test passes.
#[test]
fn removing_an_inline_map_keeps_the_comment_that_stands_apart() {
    let text = "[[providers]]\nname = \"a\"\nkind = \"mock\"\n\n\
                # Extra headers for the corporate proxy.\n\n\
                headers = { X-A = \"1\" }\npriority = 3\n";
    let out = edit(text, |c| c.providers[0].headers.clear());
    assert_eq!(
        out,
        "[[providers]]\nname = \"a\"\nkind = \"mock\"\n\n\
         # Extra headers for the corporate proxy.\n\n\
         priority = 3\n"
    );
}

/// One of several dotted keys is removed — the last one, with a comment
/// paragraph standing apart above it. A dotted table has no position, so the
/// kept comment is filed under position 0 and re-attached to the *first table
/// of the file*: it jumps from the provider to the top of the document, above
/// `[server]`, and the file now starts with a blank line. It belongs where it
/// was: after the provider's remaining keys, in front of the next section.
#[test]
fn a_comment_kept_from_a_removed_dotted_key_stays_where_it_was() {
    let text = "[server]\nport = 1\n\n\
                [[providers]]\nname = \"a\"\nkind = \"mock\"\npriority = 3\n\
                headers.X-A = \"1\"\n\n\
                # Note about B.\n\n\
                headers.X-B = \"2\"\n\n\
                [usage]\nenabled = false\n";
    let out = edit(text, |c| {
        c.providers[0].headers.shift_remove("X-B");
    });
    assert_eq!(
        out,
        "[server]\nport = 1\n\n\
         [[providers]]\nname = \"a\"\nkind = \"mock\"\npriority = 3\n\
         headers.X-A = \"1\"\n\n\
         # Note about B.\n\n\
         [usage]\nenabled = false\n"
    );
}

/// The file of the inline-map control above, with the map written as dotted
/// keys and removed as a whole: the same comment is deleted.
#[test]
fn removing_a_map_written_with_dotted_keys_keeps_the_comment_that_stands_apart() {
    let text = "[[providers]]\nname = \"a\"\nkind = \"mock\"\n\n\
                # Extra headers for the corporate proxy.\n\n\
                headers.X-A = \"1\"\nheaders.X-B = \"2\"\npriority = 3\n";
    let out = edit(text, |c| c.providers[0].headers.clear());
    assert_eq!(
        out,
        "[[providers]]\nname = \"a\"\nkind = \"mock\"\n\n\
         # Extra headers for the corporate proxy.\n\n\
         priority = 3\n"
    );
}
