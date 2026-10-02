//! Regression tests from the review: comments that ended up on the wrong
//! line, were lost, or were pushed out of place by a format-preserving update.
//!
//! The brief: "a changed scalar keeps its key's decor …; a new key is
//! appended to its table …; a removed key disappears with its own same-line
//! comment but not the comments of neighbours", and "a user who wrote … a
//! table as an inline table keeps that style".
//!
//! The parser in use accepts TOML 1.1, where an inline table may span lines,
//! carry a trailing comma and hold comments — the natural way to write a
//! provider's `headers`. The merge used to treat inline tables as one-liners.
//!
//! The first five tests are the reviewer's reproductions; the rest cover the
//! same family of mistakes in neighbouring layouts.

use switchyard_config_store::merge::{Strategy, render_update};
use switchyard_config_store::validate_text;
use switchyard_core::Config;
use switchyard_core::config::{ProviderConfig, ProviderKind};

const HEADERS: &str = r#"[[providers]]
name = "a"
kind = "mock"
headers = {
  "X-Title" = "a",  # shown in the upstream's dashboard
  "X-Other" = "b",  # required by the proxy
}
"#;

fn edit(text: &str, change: impl FnOnce(&mut Config)) -> String {
    let mut config = validate_text(text).expect("base text must be valid");
    change(&mut config);
    assert!(config.validate().is_empty());
    let rendered = render_update(text, &config).expect("render");
    assert_eq!(rendered.strategy, Strategy::Merged, "{}", rendered.text);
    assert_eq!(validate_text(&rendered.text).unwrap(), config);
    rendered.text
}

/// Removing the last key of a multi-line inline table deletes the comment of
/// the key *before* it and leaves the removed key's own comment behind, now
/// describing the wrong header.
#[test]
fn removing_a_key_from_a_multi_line_inline_table_takes_only_its_own_comment() {
    let out = edit(HEADERS, |c| {
        c.providers[0].headers.shift_remove("X-Other");
    });
    assert!(
        out.contains("\"X-Title\" = \"a\",  # shown in the upstream's dashboard\n"),
        "the neighbour's line and comment must be untouched:\n{out}"
    );
    assert!(
        !out.contains("required by the proxy"),
        "the removed key's own comment must go with it:\n{out}"
    );
}

/// Removing the first key leaves its comment behind on the line of the
/// opening brace.
#[test]
fn removing_the_first_key_of_a_multi_line_inline_table_takes_its_comment() {
    let out = edit(HEADERS, |c| {
        c.providers[0].headers.shift_remove("X-Title");
    });
    assert!(
        !out.contains("shown in the upstream's dashboard"),
        "the removed key's own comment must go with it:\n{out}"
    );
    assert!(
        out.contains("  \"X-Other\" = \"b\",  # required by the proxy\n"),
        "{out}"
    );
}

/// Adding a key squeezes it onto the last key's line, between that key and
/// its comment, so the comment now trails the new key.
#[test]
fn adding_a_key_to_a_multi_line_inline_table_leaves_the_other_lines_alone() {
    let out = edit(HEADERS, |c| {
        c.providers[0]
            .headers
            .insert("X-New".to_string(), "n".to_string());
    });
    for line in [
        "  \"X-Title\" = \"a\",  # shown in the upstream's dashboard\n",
        "  \"X-Other\" = \"b\",  # required by the proxy\n",
    ] {
        assert!(out.contains(line), "existing line changed:\n{out}");
    }
}

/// The brief: lists of tables are matched by identity, and "elements without
/// a unique identity fall back to index". Payload rules have no identity
/// key. The merge pairs them by equal content or by "at least half the fields
/// equal" and otherwise treats an edited rule as one rule deleted and another
/// inserted — so editing a rule's pattern and its value in one save (the
/// dashboard's rule form has exactly these two fields) throws away the
/// comment above the rule and the comments on its lines, although the list
/// has the same length and the rule is still the first one.
#[test]
fn a_payload_rule_edited_in_place_keeps_its_comments() {
    let text = "# force summaries\n[[payload.override]]\nmodels = [\"gpt-*\"]\nset = { \"reasoning.summary\" = \"auto\" } # keep\n\n# second\n[[payload.override]]\nmodels = [\"o3*\"]\nset = { x = 1 }\n";
    let out = edit(text, |c| {
        let rule = &mut c.payload.overrides[0];
        rule.models = vec!["gpt-5*".to_string()];
        rule.set.insert(
            "reasoning.summary".to_string(),
            serde_json::json!("detailed"),
        );
    });
    assert_eq!(
        out,
        "# force summaries\n[[payload.override]]\nmodels = [\"gpt-5*\"]\nset = { \"reasoning.summary\" = \"detailed\" } # keep\n\n# second\n[[payload.override]]\nmodels = [\"o3*\"]\nset = { x = 1 }\n"
    );
}

/// A file that so far consists of comments only (a template with everything
/// commented out, or notes left at the top of an otherwise default
/// configuration). The first edit writes its tables *above* the text that
/// opens the file and glues that text to the end of the last table. The
/// crate's own rule (see `the_comment_that_opens_the_file_survives_its_first_table`)
/// is that a new table goes below the comment that opens the file.
#[test]
fn the_first_table_of_a_comment_only_file_goes_below_the_opening_comment() {
    let text = "# Lab gateway. Ask Sam before changing anything.\n# Defaults are fine for now.\n";
    let out = edit(text, |c| {
        c.server.port = 9000;
        c.providers
            .push(ProviderConfig::new("mock", ProviderKind::Mock));
    });
    assert!(
        out.starts_with(text),
        "the comment that opens the file must stay at the top:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// The same family, neighbouring layouts
// ---------------------------------------------------------------------------

fn provider_with(headers: &str) -> String {
    format!("[[providers]]\nname = \"a\"\nkind = \"mock\"\nheaders = {headers}\npriority = 1\n")
}

/// Without a trailing comma the text before the closing brace belongs to the
/// last value; removing or adding the last key hands it over.
#[test]
fn multi_line_inline_table_without_a_trailing_comma() {
    let text = provider_with(
        "{\n  A = \"a\",  # about a\n  B = \"b\",  # about b\n  C = \"c\"  # about c\n}",
    );

    let out = edit(&text, |c| {
        c.providers[0].headers.shift_remove("C");
    });
    assert_eq!(
        out,
        provider_with("{\n  A = \"a\",  # about a\n  B = \"b\"  # about b\n}")
    );

    let out = edit(&text, |c| {
        c.providers[0].headers.shift_remove("B");
    });
    assert_eq!(
        out,
        provider_with("{\n  A = \"a\",  # about a\n  C = \"c\"  # about c\n}")
    );

    let out = edit(&text, |c| {
        c.providers[0].headers.insert("D".into(), "d".into());
        c.providers[0].headers.insert("E".into(), "e".into());
    });
    assert_eq!(
        out,
        provider_with(
            "{\n  A = \"a\",  # about a\n  B = \"b\",  # about b\n  C = \"c\",  # about c\n  D = \"d\",\n  E = \"e\"\n}"
        )
    );

    // The closing brace on the last entry's line stays there.
    let text = provider_with("{\n  A = \"a\",\n  B = \"b\" }");
    let out = edit(&text, |c| {
        c.providers[0].headers.insert("D".into(), "d".into());
    });
    assert_eq!(
        out,
        provider_with("{\n  A = \"a\",\n  B = \"b\",\n  D = \"d\" }")
    );
    let out = edit(&text, |c| {
        c.providers[0].headers.shift_remove("B");
    });
    assert_eq!(out, provider_with("{\n  A = \"a\" }"));
}

/// Comment lines above a key are about that key: they go with it and stay
/// with it.
#[test]
fn comment_lines_above_the_keys_of_an_inline_table() {
    let text = provider_with(
        "{ # the headers\n  # first\n  A = \"a\",\n  # second\n  B = \"b\",\n  # third\n  C = \"c\",\n}",
    );
    let out = edit(&text, |c| {
        c.providers[0].headers.shift_remove("B");
    });
    assert_eq!(
        out,
        provider_with("{ # the headers\n  # first\n  A = \"a\",\n  # third\n  C = \"c\",\n}")
    );
    let out = edit(&text, |c| {
        c.providers[0].headers.shift_remove("A");
        c.providers[0].headers.insert("B".into(), "bb".into());
        c.providers[0].headers.insert("D".into(), "d".into());
    });
    assert_eq!(
        out,
        provider_with(
            "{ # the headers\n  # second\n  B = \"bb\",\n  # third\n  C = \"c\",\n  D = \"d\",\n}"
        )
    );
}

/// Several keys on one line, and a table that opens on the first key's line.
#[test]
fn keys_sharing_a_line_in_a_multi_line_inline_table() {
    let text = provider_with("{\n  A = \"a\", B = \"b\",  # both\n  C = \"c\",\n}");
    let out = edit(&text, |c| {
        c.providers[0].headers.shift_remove("A");
    });
    assert_eq!(
        out,
        provider_with("{\n  B = \"b\",  # both\n  C = \"c\",\n}")
    );
    let out = edit(&text, |c| {
        c.providers[0].headers.shift_remove("C");
    });
    assert_eq!(out, provider_with("{\n  A = \"a\", B = \"b\",  # both\n}"));

    let text = provider_with("{ A = \"a\",\n  B = \"b\", C = \"c\", # tail\n}");
    let out = edit(&text, |c| {
        c.providers[0].headers.insert("D".into(), "d".into());
    });
    assert_eq!(
        out,
        provider_with("{ A = \"a\",\n  B = \"b\", C = \"c\", # tail\n  D = \"d\",\n}")
    );
    let out = edit(&text, |c| {
        c.providers[0].headers.shift_remove("B");
    });
    assert_eq!(out, provider_with("{ A = \"a\",\n  C = \"c\", # tail\n}"));
}

/// Replacing every key keeps the table's layout instead of collapsing it to
/// one line.
#[test]
fn replacing_every_key_of_a_multi_line_inline_table_keeps_its_layout() {
    let text = provider_with("{\n  A = \"a\",  # about a\n  B = \"b\",  # about b\n}");
    let out = edit(&text, |c| {
        c.providers[0].headers.clear();
        c.providers[0].headers.insert("Z".into(), "z".into());
    });
    assert_eq!(out, provider_with("{\n  Z = \"z\",\n}"));

    // Removing every key removes the setting.
    let out = edit(&text, |c| c.providers[0].headers.clear());
    assert_eq!(
        out,
        "[[providers]]\nname = \"a\"\nkind = \"mock\"\npriority = 1\n"
    );
}

#[test]
fn multi_line_inline_tables_keep_windows_line_endings() {
    let text = provider_with("{\n  A = \"a\",  # about a\n  B = \"b\",  # about b\n}")
        .replace('\n', "\r\n");
    let out = edit(&text, |c| {
        c.providers[0].headers.shift_remove("A");
        c.providers[0].headers.insert("D".into(), "d".into());
    });
    assert_eq!(
        out,
        provider_with("{\n  B = \"b\",  # about b\n  D = \"d\",\n}").replace('\n', "\r\n")
    );
}

/// Keys written with dots (`a.b = 1`) are entries of the table they are
/// written in: they share its lines and comments.
#[test]
fn dotted_keys_inside_a_multi_line_inline_table() {
    let rule = |set: &str| format!("[[payload.override]]\nmodels = [\"*\"]\nset = {set}\n");
    let text = rule("{\n  a.b = 1,  # about a.b\n  a.c = 2,  # about a.c\n  d = 3,  # about d\n}");
    let a = |c: &mut Config| {
        c.payload.overrides[0].set["a"]
            .as_object_mut()
            .expect("`a` is a table")
            .clone()
    };

    let out = edit(&text, |c| {
        let mut table = a(c);
        table.remove("c");
        c.payload.overrides[0].set["a"] = table.into();
    });
    assert_eq!(
        out,
        rule("{\n  a.b = 1,  # about a.b\n  d = 3,  # about d\n}")
    );

    let out = edit(&text, |c| {
        let mut table = a(c);
        table.insert("x".into(), serde_json::json!(9));
        c.payload.overrides[0].set["a"] = table.into();
    });
    assert_eq!(
        out,
        rule(
            "{\n  a.b = 1,  # about a.b\n  a.c = 2,  # about a.c\n  a.x = 9,\n  d = 3,  # about d\n}"
        )
    );

    let out = edit(&text, |c| {
        c.payload.overrides[0].set.shift_remove("a");
    });
    assert_eq!(out, rule("{\n  d = 3,  # about d\n}"));

    // `a` stays, without keys: it can no longer be written with dots.
    let out = edit(&text, |c| {
        c.payload.overrides[0].set["a"] = serde_json::json!({});
    });
    assert_eq!(out, rule("{\n  a = {},\n  d = 3,  # about d\n}"));

    // On one line.
    let text = rule("{ a.b = 1, a.c = 2, d = 3 }");
    let out = edit(&text, |c| {
        c.payload.overrides[0].set["a"] = serde_json::json!({ "c": 2, "x": 9 });
    });
    assert_eq!(out, rule("{ a.c = 2, a.x = 9, d = 3 }"));
}

/// A list of tables written as an inline array, one table over several
/// lines.
#[test]
fn multi_line_tables_inside_an_inline_list() {
    let text = "providers = [\n  {\n    name = \"a\",  # the first\n    kind = \"mock\",\n    headers = { X = \"1\" },\n  },\n  { name = \"b\", kind = \"mock\" },  # the second\n]\n";
    let out = edit(text, |c| {
        c.providers[0].headers.clear();
        c.providers[0].priority = 4;
        c.providers[1].priority = 5;
    });
    assert_eq!(
        out,
        "providers = [\n  {\n    name = \"a\",  # the first\n    kind = \"mock\",\n    priority = 4,\n  },\n  { name = \"b\", kind = \"mock\", priority = 5 },  # the second\n]\n"
    );
}

/// The index fallback applies to rules in every form a list can take, and
/// only to elements that have no identity.
#[test]
fn index_fallback_is_for_elements_without_identity_only() {
    // A filter rule written in an inline list.
    let text = "[payload]\nfilter = [\n  { models = [\"a*\"], remove = [\"x\"] },  # first\n  { models = [\"b*\"], remove = [\"y\"] },  # second\n]\n";
    let out = edit(text, |c| {
        c.payload.filter[1].models = vec!["c*".into()];
        c.payload.filter[1].remove = vec!["z".into()];
    });
    assert_eq!(
        out,
        "[payload]\nfilter = [\n  { models = [\"a*\"], remove = [\"x\"] },  # first\n  { models = [\"c*\"], remove = [\"z\"] },  # second\n]\n"
    );

    // A credential that is nothing but a weight is given a label and another
    // weight: still the same table, with its comments.
    let text = "[[providers]]\nname = \"local\"\nkind = \"mock\"\n\n# the heavy one\n[[providers.credentials]]\nweight = 5 # most traffic\n";
    let out = edit(text, |c| {
        c.providers[0].credentials[0].weight = Some(7);
        c.providers[0].credentials[0].label = "heavy".into();
    });
    assert_eq!(
        out,
        "[[providers]]\nname = \"local\"\nkind = \"mock\"\n\n# the heavy one\n[[providers.credentials]]\nweight = 7 # most traffic\nlabel = \"heavy\"\n"
    );

    // A provider replaced by an unrelated one at the same index is a new
    // provider: the comment about the old one must not describe it.
    let text = "# our OpenAI account\n[[providers]]\nname = \"openai\"\nkind = \"openai\"\napi_keys = [\"env:OPENAI\"] # billing: team A\n";
    let out = edit(text, |c| {
        let mut other = ProviderConfig::new("claude", ProviderKind::Anthropic);
        other.credentials = vec![switchyard_core::config::CredentialConfig {
            api_key: "env:ANTHROPIC".into(),
            ..Default::default()
        }];
        c.providers = vec![other];
    });
    assert!(!out.contains("OpenAI account"), "{out}");
    assert!(!out.contains("billing: team A"), "{out}");
}

/// Everything in a file without tables precedes the first table: notes with
/// or without a final line break, with blank lines around them, and notes
/// below values written as dotted keys.
#[test]
fn the_first_table_goes_below_whatever_the_file_holds() {
    let add = |c: &mut Config| {
        c.admin.secret = "env:X".into();
        c.providers
            .push(ProviderConfig::new("m", ProviderKind::Mock));
    };
    let tables = "[admin]\nsecret = \"env:X\"\n\n[[providers]]\nname = \"m\"\nkind = \"mock\"\n";

    assert_eq!(
        edit("# only a comment", add),
        format!("# only a comment\n\n{tables}")
    );
    assert_eq!(edit("# note\n\n", add), format!("# note\n\n{tables}"));
    assert_eq!(
        edit("server.port = 9000\n# below the values\n", add),
        format!("server.port = 9000\n# below the values\n\n{tables}")
    );
    assert_eq!(
        edit("# top\nserver.port = 9000\n\n# detached\n", add),
        format!("# top\nserver.port = 9000\n\n# detached\n\n{tables}")
    );
    // Windows line endings.
    assert_eq!(
        edit("# note\r\n# more\r\n", add),
        format!("# note\n# more\n\n{tables}").replace('\n', "\r\n")
    );
    // Going back leaves the comment where it was (the secret, once written,
    // stays in the file at its default).
    let out = edit("# note\n", add);
    let back = edit(&out, |c| *c = Config::default());
    assert_eq!(back, "# note\n\n[admin]\nsecret = \"\"\n");
}

// ---------------------------------------------------------------------------
// Property: random edits of a commented multi-line inline table touch only
// the lines of the keys they change.
// ---------------------------------------------------------------------------

/// Deterministic generator (xorshift64*), so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

struct Entry {
    key: String,
    value: String,
    above: bool,
    beside: bool,
}

#[test]
fn random_edits_of_multi_line_inline_tables_keep_every_other_line() {
    let mut rng = Rng(0x0D15_EA5E_0000_0042);
    let (mut removals, mut additions, mut changes) = (0, 0, 0);
    for case in 0..1000 {
        // The table as the user wrote it.
        let count = 1 + rng.below(5);
        let mut entries: Vec<Entry> = (0..count)
            .map(|i| Entry {
                key: format!("K{i}"),
                value: format!("v{i}"),
                above: rng.chance(30),
                beside: rng.chance(60),
            })
            .collect();
        let trailing_comma = rng.chance(50);
        let brace_comment = rng.chance(25);
        let indent = if rng.chance(50) { "  " } else { "\t" };

        let mut table = String::from("{");
        if brace_comment {
            table.push_str("  # the headers");
        }
        table.push('\n');
        for (i, entry) in entries.iter().enumerate() {
            if entry.above {
                table.push_str(&format!("{indent}# above {}\n", entry.key));
            }
            table.push_str(&format!("{indent}{} = \"{}\"", entry.key, entry.value));
            if i + 1 < entries.len() || trailing_comma {
                table.push(',');
            }
            if entry.beside {
                table.push_str(&format!("  # beside {}", entry.key));
            }
            table.push('\n');
        }
        table.push('}');
        let text = provider_with(&table);

        // The edit.
        let mut removed: Vec<String> = Vec::new();
        for entry in &mut entries {
            match rng.below(5) {
                0 => removed.push(entry.key.clone()),
                1 => entry.value = format!("{}-changed", entry.value),
                _ => {}
            }
        }
        if removed.len() == entries.len() {
            // Keep one, so the table itself stays.
            removed.pop();
        }
        let added: Vec<String> = (0..rng.below(3)).map(|i| format!("N{i}")).collect();

        let mut config = validate_text(&text).expect("generated text is valid");
        let headers = &mut config.providers[0].headers;
        for key in &removed {
            headers.shift_remove(key);
        }
        for entry in &entries {
            if !removed.contains(&entry.key) {
                headers.insert(entry.key.clone(), entry.value.clone());
            }
        }
        for key in &added {
            headers.insert(key.clone(), "new".to_string());
        }
        let unchanged = config == validate_text(&text).expect("valid");
        let rendered = render_update(&text, &config).expect("render");
        let out = rendered.text;
        let context = format!("case {case}\n--- before\n{text}\n--- after\n{out}");
        if unchanged {
            assert_eq!(rendered.strategy, Strategy::Unchanged, "{context}");
            assert_eq!(out, text, "{context}");
            continue;
        }
        assert_eq!(rendered.strategy, Strategy::Merged, "{context}");
        assert_eq!(validate_text(&out).as_ref(), Ok(&config), "{context}");
        removals += removed.len();
        additions += added.len();
        changes += 1;

        // Everything outside the table is untouched.
        assert!(
            out.starts_with("[[providers]]\nname = \"a\"\nkind = \"mock\"\nheaders = {"),
            "{context}"
        );
        assert!(out.ends_with("}\npriority = 1\n"), "{context}");
        assert_eq!(
            out.contains("{  # the headers\n"),
            brace_comment,
            "{context}"
        );

        let lines: Vec<&str> = out.lines().collect();
        let mut comments_expected = usize::from(brace_comment);
        for entry in &entries {
            let above = format!("{indent}# above {}", entry.key);
            let beside = format!("  # beside {}", entry.key);
            let start = format!("{indent}{} = \"{}\"", entry.key, entry.value);
            let at = lines.iter().position(|line| line.starts_with(&start));
            if removed.contains(&entry.key) {
                // Gone, with its own comments and nobody else's.
                assert!(
                    !lines
                        .iter()
                        .any(|l| l.starts_with(&format!("{indent}{} = ", entry.key))),
                    "{context}"
                );
                assert!(!out.contains(&above), "{context}");
                assert!(!out.contains(&beside), "{context}");
                continue;
            }
            let at = at.unwrap_or_else(|| panic!("no line for {}\n{context}", entry.key));
            // Its line: the key, the value, at most a comma, and its comment.
            let rest = &lines[at][start.len()..];
            let rest = rest.strip_prefix(',').unwrap_or(rest);
            assert_eq!(
                rest,
                if entry.beside { beside.as_str() } else { "" },
                "{context}"
            );
            // The comment above it is still directly above it.
            assert_eq!(lines[at - 1] == above, entry.above, "{context}");
            comments_expected += usize::from(entry.above) + usize::from(entry.beside);
        }
        for key in &added {
            let start = format!("{indent}{key} = \"new\"");
            let line = lines
                .iter()
                .find(|line| line.starts_with(&start))
                .unwrap_or_else(|| panic!("no line for {key}\n{context}"));
            let rest = &line[start.len()..];
            assert!(rest.is_empty() || rest == ",", "{context}");
        }
        assert_eq!(out.matches('#').count(), comments_expected, "{context}");
        // One entry per line, as before.
        let entries_now = entries.len() - removed.len() + added.len();
        assert_eq!(
            lines.len(),
            // header lines, `headers = {`, entries and their comments above,
            // `}`, `priority`
            3 + 1
                + entries_now
                + entries
                    .iter()
                    .filter(|e| e.above && !removed.contains(&e.key))
                    .count()
                + 1
                + 1,
            "{context}"
        );
    }
    // The generator really exercised all three kinds of edit.
    assert!(
        removals > 300 && additions > 300 && changes > 500,
        "{removals} {additions} {changes}"
    );
}
