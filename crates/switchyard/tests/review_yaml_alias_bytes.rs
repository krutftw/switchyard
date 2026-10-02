//! Regression test (review finding SY-BIN-3): the YAML reader's protection
//! against alias bombs must count bytes as well as nodes.
//!
//! `yaml::parse` copies an anchored node for every alias that refers to it
//! and charges the copy against a budget of 200,000 *nodes*. A long scalar
//! is one node whatever its length, so with a node budget alone a file
//! within the importer's 8 MiB limit could anchor a string of a few
//! megabytes and refer to it tens of thousands of times: every alias one
//! node and several megabytes. The document below is about 70 KiB and would
//! expand to 64 MiB (a factor of 900); the same shape at the size limit
//! would ask for hundreds of gigabytes and the process would die with an
//! allocation failure instead of an error message.
//!
//! The expansion is bounded in bytes too (16 MiB of text, the copies kept
//! for anchors included), so this document is refused.

use serde_json::Value;
use switchyard::yaml;

/// Bytes of string data the parsed tree holds.
fn string_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len(),
        Value::Array(items) => items.iter().map(string_bytes).sum(),
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| key.len() + string_bytes(value))
            .sum(),
        _ => 0,
    }
}

#[test]
fn aliases_cannot_multiply_a_long_scalar_without_limit() {
    const SCALAR: usize = 64 * 1024;
    const ALIASES: usize = 1000;

    let mut text = format!("big: &big \"{}\"\ncopies:\n", "k".repeat(SCALAR));
    for _ in 0..ALIASES {
        text.push_str("  - *big\n");
    }
    let input = text.len();
    assert!(input < 80 * 1024);

    // Either the document is refused, or reading it did not multiply it.
    if let Ok(value) = yaml::parse(&text) {
        let expanded = string_bytes(&value);
        assert!(
            expanded <= 16 * input,
            "a {input}-byte document expanded to {expanded} bytes of strings ({} times its \
             size): the alias budget counts nodes, so the copies of a long scalar are free",
            expanded / input
        );
    }
}
