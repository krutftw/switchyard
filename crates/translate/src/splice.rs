//! Replacing values inside a JSON text without touching the rest of it.
//!
//! Passthrough forwards the upstream's payloads "as-is with the model name
//! rewritten". Parsing a payload into a [`Value`] and printing it again is
//! not "as-is": it normalises whitespace and escapes, re-prints every number
//! (integers beyond 64 bits turn into floats, floats are reformatted) and
//! drops duplicate keys. [`splice`] avoids all of that: given the text, the
//! value it parsed to and the value it should become, it replaces only the
//! bytes of the values that differ and copies everything else verbatim.

use serde_json::Value;
use std::ops::Range;

/// One step from a JSON container to one of its children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step<'a> {
    Key(&'a str),
    Index(usize),
}

/// Rewrites `text`, which parsed to `old`, so that it parses to `new`, by
/// replacing only the values that differ between the two.
///
/// Returns `None` when that is not possible with in-place replacements and
/// the caller has to print `new` instead: an object on the way to a changed
/// value spells the wanted key twice (which of the two a reader honours is
/// implementation-defined, so neither can be left behind), or `text` is not
/// the JSON that `old` was parsed from.
///
/// Where `new` restructures a container (different keys, different key order,
/// different array length) that whole container is replaced; its siblings are
/// still kept verbatim.
pub(crate) fn splice(text: &str, old: &Value, new: &Value) -> Option<String> {
    let mut edits = Vec::new();
    diff(old, new, &mut Vec::new(), &mut edits);

    let mut spans = Vec::with_capacity(edits.len());
    for (path, value) in &edits {
        spans.push((locate(text, path)?, *value));
    }
    // `diff` reports edits in document order; sorting only guards the copy
    // loop below against a container that lists its keys out of order.
    spans.sort_by_key(|(span, _)| span.start);

    let mut out = String::with_capacity(text.len() + 32);
    let mut copied = 0;
    for (span, value) in spans {
        if span.start < copied {
            return None;
        }
        out.push_str(text.get(copied..span.start)?);
        out.push_str(&serde_json::to_string(value).ok()?);
        copied = span.end;
    }
    out.push_str(text.get(copied..)?);
    Some(out)
}

/// Collects the smallest set of in-place replacements that turn `old` into
/// `new`: it descends as long as both sides are containers of the same shape
/// and reports the first node that is not. Only called for `old != new`.
fn diff<'a>(
    old: &'a Value,
    new: &'a Value,
    path: &mut Vec<Step<'a>>,
    edits: &mut Vec<(Vec<Step<'a>>, &'a Value)>,
) {
    match (old, new) {
        (Value::Object(before), Value::Object(after))
            if before.len() == after.len() && before.keys().eq(after.keys()) =>
        {
            for ((key, was), now) in before.iter().zip(after.values()) {
                if was != now {
                    path.push(Step::Key(key));
                    diff(was, now, path, edits);
                    path.pop();
                }
            }
        }
        (Value::Array(before), Value::Array(after)) if before.len() == after.len() => {
            for (index, (was, now)) in before.iter().zip(after).enumerate() {
                if was != now {
                    path.push(Step::Index(index));
                    diff(was, now, path, edits);
                    path.pop();
                }
            }
        }
        _ => edits.push((path.clone(), new)),
    }
}

/// A position in a JSON text. Every method returns `None` instead of reading
/// past the end, so malformed input can never cause a panic.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Option<()> {
        if self.peek()? == byte {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    /// Consumes a string starting at its opening quote and returns the span
    /// of its contents (without the quotes, escapes still encoded).
    fn string(&mut self) -> Option<Range<usize>> {
        self.expect(b'"')?;
        let start = self.pos;
        loop {
            match self.peek()? {
                b'"' => {
                    let end = self.pos;
                    self.pos += 1;
                    return Some(start..end);
                }
                // Whatever is escaped, it is not the closing quote.
                b'\\' => self.pos += 2,
                _ => self.pos += 1,
            }
        }
    }

    /// Consumes one value of any kind.
    fn value(&mut self) -> Option<()> {
        match self.peek()? {
            b'"' => {
                self.string()?;
            }
            b'{' | b'[' => {
                // Brackets inside strings are skipped with their string, so
                // counting the rest finds the matching close without
                // recursion.
                let mut depth = 0usize;
                loop {
                    match self.peek()? {
                        b'"' => {
                            self.string()?;
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth = depth.checked_sub(1)?;
                            if depth == 0 {
                                self.pos += 1;
                                return Some(());
                            }
                        }
                        _ => {}
                    }
                    self.pos += 1;
                }
            }
            _ => {
                // A number or a literal: runs up to the next delimiter.
                let start = self.pos;
                while let Some(byte) = self.peek() {
                    if matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    self.pos += 1;
                }
                if self.pos == start {
                    return None;
                }
            }
        }
        Some(())
    }
}

/// The byte span of the value at `path` in `text`.
fn locate(text: &str, path: &[Step<'_>]) -> Option<Range<usize>> {
    let mut cursor = Cursor {
        bytes: text.as_bytes(),
        pos: 0,
    };
    cursor.skip_whitespace();
    for step in path {
        match *step {
            Step::Key(wanted) => {
                cursor.expect(b'{')?;
                let mut found = None;
                loop {
                    cursor.skip_whitespace();
                    if cursor.peek()? == b'}' {
                        break;
                    }
                    let key = cursor.string()?;
                    cursor.skip_whitespace();
                    cursor.expect(b':')?;
                    cursor.skip_whitespace();
                    if key_is(text, key, wanted)? {
                        if found.is_some() {
                            // The key is spelled twice: see `splice`.
                            return None;
                        }
                        found = Some(cursor.pos);
                    }
                    cursor.value()?;
                    cursor.skip_whitespace();
                    match cursor.peek()? {
                        b',' => cursor.pos += 1,
                        b'}' => break,
                        _ => return None,
                    }
                }
                cursor.pos = found?;
            }
            Step::Index(wanted) => {
                cursor.expect(b'[')?;
                for _ in 0..wanted {
                    cursor.skip_whitespace();
                    cursor.value()?;
                    cursor.skip_whitespace();
                    cursor.expect(b',')?;
                }
                cursor.skip_whitespace();
            }
        }
    }
    let start = cursor.pos;
    cursor.value()?;
    Some(start..cursor.pos)
}

/// Whether the object key whose encoded contents are at `span` is `wanted`.
fn key_is(text: &str, span: Range<usize>, wanted: &str) -> Option<bool> {
    let raw = text.get(span.clone())?;
    if !raw.contains('\\') {
        return Some(raw == wanted);
    }
    // Escapes: let the real parser decode the key, quotes included.
    let quoted = text.get(span.start.checked_sub(1)?..span.end.checked_add(1)?)?;
    let decoded: String = serde_json::from_str(quoted).ok()?;
    Some(decoded == wanted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    /// Parses `text`, lets `change` edit the value, splices.
    fn edit(text: &str, change: impl FnOnce(&mut Value)) -> Option<String> {
        let old: Value = serde_json::from_str(text).expect("test input is JSON");
        let mut new = old.clone();
        change(&mut new);
        splice(text, &old, &new)
    }

    #[test]
    fn replaces_only_the_changed_value() {
        let text =
            r#"{ "id" : "x",  "model" : "up", "n": 1.50, "big": 123456789012345678901234567890 }"#;
        assert_eq!(
            edit(text, |v| v["model"] = json!("alias")).as_deref(),
            Some(
                r#"{ "id" : "x",  "model" : "alias", "n": 1.50, "big": 123456789012345678901234567890 }"#
            )
        );
    }

    #[test]
    fn keeps_numbers_escapes_and_whitespace_byte_for_byte() {
        let text = "{\"a\":-1.6596847772598267,\"b\":1E5,\"c\":\"\\u00e9\\/\",\n\t\"model\":\"up\",\"d\":-0,\"e\":0.10}";
        assert_eq!(
            edit(text, |v| v["model"] = json!("alias")).as_deref(),
            Some(
                "{\"a\":-1.6596847772598267,\"b\":1E5,\"c\":\"\\u00e9\\/\",\n\t\"model\":\"alias\",\"d\":-0,\"e\":0.10}"
            )
        );
    }

    #[test]
    fn replaces_nested_values() {
        let text = r#"{"type":"x","response":{"id":"r","model":"up","output":[{"model":"keep"}]},"model":"up"}"#;
        assert_eq!(
            edit(text, |v| {
                v["response"]["model"] = json!("alias");
                v["model"] = json!("alias");
            })
            .as_deref(),
            Some(
                r#"{"type":"x","response":{"id":"r","model":"alias","output":[{"model":"keep"}]},"model":"alias"}"#
            )
        );
    }

    #[test]
    fn replaces_values_inside_arrays() {
        let text = r#"[ {"modelVersion":"a"} , [1, 2], {"modelVersion":"b","x":[ ]} ]"#;
        assert_eq!(
            edit(text, |v| {
                v[0]["modelVersion"] = json!("alias");
                v[2]["modelVersion"] = json!("alias");
            })
            .as_deref(),
            Some(r#"[ {"modelVersion":"alias"} , [1, 2], {"modelVersion":"alias","x":[ ]} ]"#)
        );
        assert_eq!(
            edit("[1,[2,3],4]", |v| v[1][1] = json!(null)).as_deref(),
            Some("[1,[2,null],4]")
        );
    }

    #[test]
    fn the_replacement_is_escaped_json() {
        assert_eq!(
            edit(r#"{"model":"up"}"#, |v| v["model"] = json!("a\"b\\c\n")).as_deref(),
            Some(r#"{"model":"a\"b\\c\n"}"#)
        );
    }

    #[test]
    fn a_value_can_change_its_type() {
        assert_eq!(
            edit(r#"{"a": "text", "b": 2}"#, |v| v["a"] = json!({"k": [1]})).as_deref(),
            Some(r#"{"a": {"k":[1]}, "b": 2}"#)
        );
        assert_eq!(
            edit(r#"{"a": {"k": [1, 2]}, "b": 2}"#, |v| v["a"] = json!(7)).as_deref(),
            Some(r#"{"a": 7, "b": 2}"#)
        );
    }

    #[test]
    fn a_restructured_container_is_replaced_as_a_whole() {
        // A key was added to the inner object: that object is reprinted, its
        // siblings are not.
        let text = r#"{ "keep": 1.50, "inner": { "a": 1 }, "tail": [ 1 ] }"#;
        assert_eq!(
            edit(text, |v| v["inner"]["b"] = json!(2)).as_deref(),
            Some(r#"{ "keep": 1.50, "inner": {"a":1,"b":2}, "tail": [ 1 ] }"#)
        );
        // An array that changed its length likewise.
        assert_eq!(
            edit(r#"{"list": [ 1, 2 ], "z": 0}"#, |v| {
                v["list"].as_array_mut().unwrap().push(json!(3));
            })
            .as_deref(),
            Some(r#"{"list": [1,2,3], "z": 0}"#)
        );
    }

    #[test]
    fn a_restructured_root_is_reprinted_with_its_surrounding_whitespace() {
        assert_eq!(
            edit(" { \"a\": 1 } ", |v| v["b"] = json!(2)).as_deref(),
            Some(" {\"a\":1,\"b\":2} ")
        );
        assert_eq!(
            edit(" 12 ", |v| *v = json!("x")).as_deref(),
            Some(" \"x\" ")
        );
    }

    #[test]
    fn escaped_keys_are_decoded_before_comparing() {
        // "model" is the key "model".
        let text = r#"{"model":"up","other\"key":1}"#;
        assert_eq!(
            edit(text, |v| v["model"] = json!("alias")).as_deref(),
            Some(r#"{"model":"alias","other\"key":1}"#)
        );
        assert_eq!(
            edit(text, |v| v["other\"key"] = json!(2)).as_deref(),
            Some(r#"{"model":"up","other\"key":2}"#)
        );
    }

    #[test]
    fn brackets_and_quotes_inside_strings_do_not_confuse_the_scanner() {
        let text = r#"{"a":"}]{[\"","b":{"c":"]}","d":["\\"]},"model":"up"}"#;
        assert_eq!(
            edit(text, |v| v["model"] = json!("alias")).as_deref(),
            Some(r#"{"a":"}]{[\"","b":{"c":"]}","d":["\\"]},"model":"alias"}"#)
        );
    }

    #[test]
    fn non_ascii_text_is_copied_intact() {
        let text = r#"{"t":"héllo → 世界 🎉","model":"up","u":"é"}"#;
        assert_eq!(
            edit(text, |v| v["model"] = json!("ålias")).as_deref(),
            Some(r#"{"t":"héllo → 世界 🎉","model":"ålias","u":"é"}"#)
        );
    }

    #[test]
    fn a_duplicated_key_on_the_way_is_not_spliced() {
        assert_eq!(
            edit(r#"{"model":"a","model":"b"}"#, |v| v["model"] =
                json!("alias")),
            None
        );
        assert_eq!(
            edit(r#"{"r":{"model":"a"},"r":{"model":"b"}}"#, |v| {
                v["r"]["model"] = json!("alias");
            }),
            None
        );
        // A duplicate elsewhere is none of our business and is kept.
        assert_eq!(
            edit(r#"{"x":1,"x":2,"model":"up"}"#, |v| v["model"] =
                json!("alias"))
            .as_deref(),
            Some(r#"{"x":1,"x":2,"model":"alias"}"#)
        );
    }

    #[test]
    fn text_that_does_not_match_the_value_is_refused_without_panicking() {
        let old = json!({"model": "up", "list": [1, 2, {"k": "v"}]});
        let mut new = old.clone();
        new["model"] = json!("alias");
        new["list"][2]["k"] = json!("w");
        for text in [
            "",
            "   ",
            "{",
            "{\"model\"",
            "{\"model\":",
            "{\"model\":\"up",
            "{\"model\":\"up\\",
            "{\"model\":\"up\",\"list\":[1",
            "{\"model\":\"up\",\"list\":[1,2]}",
            "{\"model\":\"up\",\"list\":[1,2,{}]}",
            "{\"other\":1}",
            "[1,2,3]",
            "\"model\"",
            "{\"model\" \"up\"}",
            "{\"model\":\"up\" \"list\":[]}",
            "{\"list\":[1,2,{\"k\":]}",
            "}",
            "]",
        ] {
            assert_eq!(splice(text, &old, &new), None, "{text:?}");
        }
    }

    #[test]
    fn identical_values_need_no_edit() {
        let text = r#"{ "a": 1 }"#;
        assert_eq!(edit(text, |_| {}).as_deref(), Some(text));
    }

    #[test]
    fn deep_nesting_is_scanned_without_recursion() {
        // Deeper than serde_json's own limit: the scanner must still skip it.
        let deep = format!("{}{}", "[".repeat(5_000), "]".repeat(5_000));
        let text = format!(r#"{{"skip":{deep},"model":"up"}}"#);
        let old = json!({"skip": null, "model": "up"});
        let new = json!({"skip": null, "model": "alias"});
        assert_eq!(
            splice(&text, &old, &new),
            Some(format!(r#"{{"skip":{deep},"model":"alias"}}"#))
        );
    }

    // ----- randomised ---------------------------------------------------------

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    const KEYS: [&str; 6] = ["model", "a", "k\"q", "é", "", "x\\y"];
    const STRINGS: [&str; 6] = ["", "up", "}],{[:\"", "\\", "日本\n", "model"];

    fn random_value(rng: &mut Rng, depth: u32) -> Value {
        match rng.below(if depth == 0 { 5 } else { 7 }) {
            0 => Value::Null,
            1 => json!(rng.below(2) == 0),
            2 => json!(rng.next() as i64),
            3 => json!((rng.next() as f64) / 7.0e11),
            4 => json!(STRINGS[rng.below(6) as usize]),
            5 => Value::Array(
                (0..rng.below(4))
                    .map(|_| random_value(rng, depth - 1))
                    .collect(),
            ),
            _ => Value::Object(
                (0..rng.below(4))
                    .map(|_| {
                        (
                            KEYS[rng.below(6) as usize].to_string(),
                            random_value(rng, depth - 1),
                        )
                    })
                    .collect(),
            ),
        }
    }

    fn gap(rng: &mut Rng, out: &mut String) {
        out.push_str(["", "", " ", "\n", "\t ", " \r\n "][rng.below(6) as usize]);
    }

    /// Prints `value` with random whitespace between its tokens.
    fn print(value: &Value, rng: &mut Rng, out: &mut String) {
        match value {
            Value::Array(items) => {
                out.push('[');
                for (n, item) in items.iter().enumerate() {
                    if n > 0 {
                        out.push(',');
                    }
                    gap(rng, out);
                    print(item, rng, out);
                    gap(rng, out);
                }
                gap(rng, out);
                out.push(']');
            }
            Value::Object(map) => {
                out.push('{');
                for (n, (key, item)) in map.iter().enumerate() {
                    if n > 0 {
                        out.push(',');
                    }
                    gap(rng, out);
                    out.push_str(&serde_json::to_string(key).unwrap());
                    gap(rng, out);
                    out.push(':');
                    gap(rng, out);
                    print(item, rng, out);
                    gap(rng, out);
                }
                gap(rng, out);
                out.push('}');
            }
            scalar => out.push_str(&scalar.to_string()),
        }
    }

    /// Replaces a few randomly chosen nodes of `value`.
    fn mutate(value: &mut Value, rng: &mut Rng) {
        match value {
            Value::Array(items) if !items.is_empty() && rng.below(3) > 0 => {
                let at = rng.below(items.len() as u64) as usize;
                mutate(&mut items[at], rng);
            }
            Value::Object(map) if !map.is_empty() && rng.below(3) > 0 => {
                let at = rng.below(map.len() as u64) as usize;
                if let Some(child) = map.values_mut().nth(at) {
                    mutate(child, rng);
                }
            }
            other => *other = random_value(rng, 2),
        }
    }

    #[test]
    fn spliced_text_parses_to_the_new_value_and_keeps_the_rest() {
        let mut rng = Rng(0x5EED_1234_ABCD_EF01);
        let mut spliced_some = 0;
        for round in 0..20_000 {
            let old = random_value(&mut rng, 4);
            let mut text = String::new();
            gap(&mut rng, &mut text);
            print(&old, &mut rng, &mut text);
            gap(&mut rng, &mut text);
            assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), old);

            let mut new = old.clone();
            for _ in 0..1 + rng.below(3) {
                mutate(&mut new, &mut rng);
            }
            // Objects never repeat a key here, so splicing always works.
            let out = splice(&text, &old, &new)
                .unwrap_or_else(|| panic!("round {round}: no splice for {text:?}"));
            let parsed: Value = serde_json::from_str(&out)
                .unwrap_or_else(|e| panic!("round {round}: {e}: {out:?} from {text:?}"));
            assert_eq!(parsed, new, "round {round}: {out:?} from {text:?}");

            if old == new {
                assert_eq!(out, text, "round {round}");
                continue;
            }
            spliced_some += 1;
            // What surrounds the edits is the original text: the common
            // prefix and suffix of the two documents cover everything
            // outside the first and the last changed value.
            let mut edits = Vec::new();
            diff(&old, &new, &mut Vec::new(), &mut edits);
            let first = locate(&text, &edits[0].0).unwrap();
            let last = locate(&text, &edits[edits.len() - 1].0).unwrap();
            assert!(out.starts_with(&text[..first.start]), "round {round}");
            assert!(out.ends_with(&text[last.end..]), "round {round}");
        }
        assert!(spliced_some > 10_000);
    }

    #[test]
    fn locate_finds_spans() {
        let text = r#" {"a": [10, {"b": "x"} ], "c": null} "#;
        let span = |path: &[Step<'_>]| locate(text, path).map(|s| &text[s]);
        assert_eq!(span(&[]), Some(r#"{"a": [10, {"b": "x"} ], "c": null}"#));
        assert_eq!(span(&[Step::Key("a")]), Some(r#"[10, {"b": "x"} ]"#));
        assert_eq!(span(&[Step::Key("a"), Step::Index(0)]), Some("10"));
        assert_eq!(
            span(&[Step::Key("a"), Step::Index(1), Step::Key("b")]),
            Some("\"x\"")
        );
        assert_eq!(span(&[Step::Key("c")]), Some("null"));
        assert_eq!(span(&[Step::Key("missing")]), None);
        assert_eq!(span(&[Step::Key("a"), Step::Index(2)]), None);
        assert_eq!(span(&[Step::Index(0)]), None);
        assert_eq!(span(&[Step::Key("c"), Step::Key("d")]), None);
    }
}
