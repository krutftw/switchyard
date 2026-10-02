//! What happens to comments when the thing below them is removed.
//!
//! The rule (see the `merge` module documentation): a comment on an item's
//! own line and the comment lines directly above it go with the item; a
//! comment paragraph set apart by a blank line belongs to the place and
//! stays. These tests pin how that rule applies when what is removed is
//! bigger than one key or one header:
//!
//! * a *section* that merely groups other tables (`payload`) — every header
//!   below it is treated on its own;
//! * a *list element* (`[[providers]]`) — its block goes as one;
//! * a map written with *dotted keys* — each key is a line of the table it is
//!   written in.
//!
//! Companion to `review_removed_section_comments.rs`, which holds the
//! reviewer's reproductions of the original defect.

use pretty_assertions::assert_eq;
use switchyard_config_store::merge::{Strategy, render_update};
use switchyard_config_store::validate_text;
use switchyard_core::Config;

fn edit(text: &str, change: impl FnOnce(&mut Config)) -> String {
    let mut config = validate_text(text).expect("valid base");
    change(&mut config);
    assert!(config.validate().is_empty(), "{:?}", config.validate());
    let rendered = render_update(text, &config).expect("render");
    assert_eq!(rendered.strategy, Strategy::Merged);
    assert_eq!(validate_text(&rendered.text).expect("valid output"), config);
    // Doing it again changes nothing.
    let again = render_update(&rendered.text, &config).expect("render again");
    assert_eq!(again.strategy, Strategy::Unchanged);
    rendered.text
}

// -- sections ---------------------------------------------------------------

/// `[payload]` written out, with a banner of its own and one above its list.
/// Both banners stay when the last rule goes.
#[test]
fn an_explicit_payload_header_and_its_lists_leave_their_banners() {
    let text = "[server]\nport = 1\n\n\
                # Banner payload.\n\n\
                [payload]\n\n\
                # Banner filter.\n\n\
                # strips metadata\n\
                [[payload.filter]]   # everywhere\n\
                models = [\"a\"]\nremove = [\"x\"]\n\n\
                # Banner usage.\n\n\
                [usage]\nenabled = false\n";
    let out = edit(text, |c| c.payload = Default::default());
    assert_eq!(
        out,
        "[server]\nport = 1\n\n\
         # Banner payload.\n\n\
         # Banner filter.\n\n\
         # Banner usage.\n\n\
         [usage]\nenabled = false\n"
    );
}

/// Payload rules are all the file has: the kept banners become the text that
/// ends the file.
#[test]
fn banners_of_the_last_tables_of_a_file_stay_at_its_end() {
    let text = "# Intro.\n\n\
                # Banner filter.\n\n\
                [[payload.filter]]\nmodels = [\"a\"]\nremove = [\"x\"]\n\n\
                # closing words\n";
    let out = edit(text, |c| c.payload = Default::default());
    assert_eq!(out, "# Intro.\n\n# Banner filter.\n\n# closing words\n");
}

/// One list goes, another stays: nothing new, but the two cases must agree.
#[test]
fn one_emptied_payload_list_leaves_its_banner() {
    let text = "# Banner A.\n\n\
                [[payload.filter]]\nmodels = [\"a\"]\nremove = [\"x\"]\n\n\
                # Banner B.\n\n\
                [[payload.default]]\nmodels = [\"a\"]\nset = { x = 1 }\n";
    let out = edit(text, |c| c.payload.filter.clear());
    assert_eq!(
        out,
        "# Banner A.\n\n\
         # Banner B.\n\n\
         [[payload.default]]\nmodels = [\"a\"]\nset = { x = 1 }\n"
    );
}

/// Windows line endings survive the detour through the kept comments.
#[test]
fn kept_banners_keep_windows_line_endings() {
    let text = "[server]\r\nport = 1\r\n\r\n\
                # Banner payload.\r\n\r\n\
                [[payload.filter]]\r\nmodels = [\"a\"]\r\nremove = [\"x\"]\r\n\r\n\
                # Banner usage.\r\n\r\n\
                [usage]\r\nenabled = false\r\n";
    let out = edit(text, |c| c.payload = Default::default());
    assert_eq!(
        out,
        "[server]\r\nport = 1\r\n\r\n\
         # Banner payload.\r\n\r\n\
         # Banner usage.\r\n\r\n\
         [usage]\r\nenabled = false\r\n"
    );
}

// -- list elements ----------------------------------------------------------

/// A deleted provider is deleted as a block: its keys, its sub-tables, and
/// every comment between its header and the end of its last sub-table. The
/// banner above it and the one above the next section are not its own.
#[test]
fn a_removed_list_element_takes_its_whole_block() {
    let text = "[server]\nport = 1\n\n\
                # Banner providers.\n\n\
                # about a\n\
                [[providers]]\nname = \"a\"\nkind = \"mock\"\n\n\
                # between the keys of a\n\n\
                priority = 3\n\n\
                # Models of a.\n\n\
                # first model\n\
                [[providers.models]]\nid = \"m1\"\n\n\
                # its thinking\n\n\
                [providers.models.thinking]\nlevels = [\"low\"]\n\n\
                # Banner usage.\n\n\
                [usage]\nenabled = false\n";
    let out = edit(text, |c| c.providers.clear());
    assert_eq!(
        out,
        "[server]\nport = 1\n\n\
         # Banner providers.\n\n\
         # Banner usage.\n\n\
         [usage]\nenabled = false\n"
    );
}

/// The same for an element one level down: a model and the sub-table that
/// follows it.
#[test]
fn a_removed_nested_element_takes_its_block_and_leaves_the_list_banner() {
    let text = "[[providers]]\nname = \"a\"\nkind = \"mock\"\n\n\
                # Models.\n\n\
                [[providers.models]]\nid = \"m\"\n\n\
                # Thinking of m.\n\n\
                [providers.models.thinking]\nlevels = [\"low\"]\n\n\
                # Second.\n\n\
                [[providers.models]]\nid = \"n\"\n";
    let out = edit(text, |c| {
        c.providers[0].models.remove(0);
    });
    assert_eq!(
        out,
        "[[providers]]\nname = \"a\"\nkind = \"mock\"\n\n\
         # Models.\n\n\
         # Second.\n\n\
         [[providers.models]]\nid = \"n\"\n"
    );
}

/// TOML lets a sub-table of an element stand further down the file, after
/// other sections. Such a header is not part of the element's block: the
/// paragraph above it sits between two sections that stay, and stays too.
#[test]
fn a_sub_table_written_elsewhere_in_the_file_leaves_its_banner() {
    let text = "[[providers]]\nname = \"a\"\nkind = \"mock\"\n\n\
                # in the block\n\n\
                [[providers.models]]\nid = \"m1\"\n\n\
                # Aliases.\n\n\
                [[aliases]]\nname = \"x\"\ntargets = [\"m\"]\n\n\
                # Far model banner.\n\n\
                # about m\n\
                [[providers.models]]\nid = \"m\"\n\n\
                # Usage.\n\n\
                [usage]\nenabled = false\n";
    let out = edit(text, |c| c.providers.clear());
    assert_eq!(
        out,
        "# Aliases.\n\n\
         [[aliases]]\nname = \"x\"\ntargets = [\"m\"]\n\n\
         # Far model banner.\n\n\
         # Usage.\n\n\
         [usage]\nenabled = false\n"
    );
}

/// The last client key goes while `[auth]` stays.
#[test]
fn the_last_element_of_a_list_in_a_kept_section_leaves_its_banner() {
    let text = "[auth]\nrequired = false\n\n\
                # Keys banner.\n\n\
                # laptop\n\
                [[auth.keys]]\nkey = \"k1\"\n\n\
                # Usage.\n\n\
                [usage]\nenabled = false\n";
    let out = edit(text, |c| c.auth.keys.clear());
    assert_eq!(
        out,
        "[auth]\nrequired = false\n\n\
         # Keys banner.\n\n\
         # Usage.\n\n\
         [usage]\nenabled = false\n"
    );
}

// -- dotted keys ------------------------------------------------------------

/// Several dotted keys of one map are removed. Each is a line of the
/// provider's table: the paragraph above it moves in front of the next line
/// that is left.
#[test]
fn removed_dotted_keys_leave_the_paragraphs_above_them() {
    let text = "[[providers]]\nname = \"a\"\nkind = \"mock\"\n\n\
                # Note A.\n\n\
                headers.X-A = \"1\"\n\n\
                # Note B.\n\n\
                # about B\n\
                headers.X-B = \"2\" # b\n\n\
                # Note C.\n\n\
                headers.X-C = \"3\"\n\n\
                # Note priority.\n\n\
                priority = 3\n";
    let out = edit(text, |c| {
        c.providers[0].headers.shift_remove("X-A");
        c.providers[0].headers.shift_remove("X-C");
    });
    assert_eq!(
        out,
        "[[providers]]\nname = \"a\"\nkind = \"mock\"\n\n\
         # Note A.\n\n\
         # Note B.\n\n\
         # about B\n\
         headers.X-B = \"2\" # b\n\n\
         # Note C.\n\n\
         # Note priority.\n\n\
         priority = 3\n"
    );
}

/// A whole map of dotted keys goes: every paragraph that stood apart stays,
/// the comments attached to the keys go.
#[test]
fn a_removed_map_of_dotted_keys_leaves_every_paragraph_that_stood_apart() {
    let text = "[server]\nport = 9000\n\n\
                # TLS note.\n\n\
                tls.cert = \"c\" # the certificate\n\n\
                # Key note.\n\n\
                # readable by the service only\n\
                tls.key = \"k\"\n\n\
                [usage]\nenabled = false\n";
    let out = edit(text, |c| c.server.tls = None);
    assert_eq!(
        out,
        "[server]\nport = 9000\n\n\
         # TLS note.\n\n\
         # Key note.\n\n\
         [usage]\nenabled = false\n"
    );
}

/// Dotted keys at the top of the file, before any header. What they leave
/// goes in front of the next top-level key, or — when they were the last
/// ones — in front of the first header.
#[test]
fn dotted_keys_at_the_top_of_the_file_leave_their_paragraphs_in_place() {
    let text = "# Intro.\n\n\
                server.port = 9000\n\n\
                # TLS note.\n\n\
                server.tls.cert = \"c\"\nserver.tls.key = \"k\"\n\n\
                # Admin note.\n\n\
                admin.secret = \"x\"\n\n\
                [usage]\nenabled = false\n";
    let out = edit(text, |c| c.server.tls = None);
    assert_eq!(
        out,
        "# Intro.\n\n\
         server.port = 9000\n\n\
         # TLS note.\n\n\
         # Admin note.\n\n\
         admin.secret = \"x\"\n\n\
         [usage]\nenabled = false\n"
    );

    let last = "# Intro.\n\n\
                server.port = 9000\n\n\
                # TLS note.\n\n\
                server.tls.cert = \"c\"\nserver.tls.key = \"k\"\n\n\
                [usage]\nenabled = false\n";
    let out = edit(last, |c| c.server.tls = None);
    assert_eq!(
        out,
        "# Intro.\n\n\
         server.port = 9000\n\n\
         # TLS note.\n\n\
         [usage]\nenabled = false\n"
    );
}

/// The line after a removed key is a dotted key: that is the line the kept
/// paragraph goes in front of. (It used to be skipped, and the paragraph
/// ended up below the map.)
#[test]
fn a_paragraph_kept_from_a_plain_key_goes_in_front_of_a_following_dotted_key() {
    let text = "[[providers]]\nname = \"a\"\nkind = \"mock\"\n\n\
                # Routing of this provider.\n\n\
                priority = 3\n\
                headers.X-A = \"1\"\n";
    let out = edit(text, |c| c.providers[0].priority = 0);
    assert_eq!(
        out,
        "[[providers]]\nname = \"a\"\nkind = \"mock\"\n\n\
         # Routing of this provider.\n\n\
         headers.X-A = \"1\"\n"
    );
}

/// The removed dotted keys were the last lines of an element that has
/// sub-tables: the paragraph ends the element's own keys, in front of its
/// first sub-table — not at the top of the file.
#[test]
fn a_paragraph_kept_from_the_last_dotted_keys_stays_inside_its_element() {
    let text = "[server]\nport = 1\n\n\
                [[providers]]\nname = \"a\"\nkind = \"mock\"\n\
                [[providers.models]]\nid = \"m\"\n\n\
                # Thinking.\n\n\
                thinking.min = 1\nthinking.max = 2\n\n\
                [[providers.models]]\nid = \"n\"\n";
    let out = edit(text, |c| c.providers[0].models[0].thinking = None);
    assert_eq!(
        out,
        "[server]\nport = 1\n\n\
         [[providers]]\nname = \"a\"\nkind = \"mock\"\n\
         [[providers.models]]\nid = \"m\"\n\n\
         # Thinking.\n\n\
         [[providers.models]]\nid = \"n\"\n"
    );
}

/// Dotted keys with Windows line endings.
#[test]
fn kept_paragraphs_of_dotted_keys_keep_windows_line_endings() {
    let text = "[[providers]]\r\nname = \"a\"\r\nkind = \"mock\"\r\n\r\n\
                # Note A.\r\n\r\n\
                headers.X-A = \"1\"\r\n\r\n\
                # Note priority.\r\n\r\n\
                priority = 3\r\n";
    let out = edit(text, |c| c.providers[0].headers.clear());
    assert_eq!(
        out,
        "[[providers]]\r\nname = \"a\"\r\nkind = \"mock\"\r\n\r\n\
         # Note A.\r\n\r\n\
         # Note priority.\r\n\r\n\
         priority = 3\r\n"
    );
}
