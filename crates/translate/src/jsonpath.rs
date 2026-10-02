//! Dotted paths into [`serde_json::Value`] trees.
//!
//! Payload rules in the configuration file address fields of an upstream
//! request body with paths such as `generationConfig.thinkingConfig.thinkingBudget`
//! or `messages.0.role`. This module implements exactly that small language
//! and nothing more (no wildcards, no queries):
//!
//! * segments are separated by `.`;
//! * `\.` is a literal dot inside a segment and `\\` a literal backslash, so
//!   every possible object key can be written (`metadata.user\.id` addresses
//!   the key `user.id` inside `metadata`); a backslash followed by anything
//!   else is kept as it is;
//! * a segment made only of ASCII digits is an **array index** when the value
//!   it is applied to is an array, and an ordinary key when that value is an
//!   object (`{"0": …}` stays addressable);
//! * an empty path addresses nothing: every operation on it is a no-op.
//!   Empty segments are legal keys (`a..b` is `a` → `""` → `b`).
//!
//! All functions are total: no input makes them panic, and a path that cannot
//! be followed (missing key, index out of range, scalar in the way) yields
//! `None` / `false` instead of an error.

use serde_json::{Map, Value};

/// One step of a parsed path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Segment {
    key: String,
    index: Option<usize>,
}

impl Segment {
    /// Builds a segment from its unescaped text.
    pub fn new(key: impl Into<String>) -> Self {
        let key = key.into();
        // `str::parse::<usize>` also accepts a leading `+`; a segment is an
        // index only when it is digits and nothing else.
        let index = if !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit()) {
            key.parse::<usize>().ok()
        } else {
            None
        };
        Segment { key, index }
    }

    /// The segment as an object key (escapes already resolved).
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The segment as an array index, when it is numeric.
    pub fn index(&self) -> Option<usize> {
        self.index
    }
}

/// Splits `path` into segments. An empty path yields no segments.
pub fn parse(path: &str) -> Vec<Segment> {
    if path.is_empty() {
        return Vec::new();
    }
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek() {
                Some('.') => {
                    current.push('.');
                    chars.next();
                }
                Some('\\') => {
                    current.push('\\');
                    chars.next();
                }
                // Not an escape we know: keep the backslash, let the next
                // character be handled normally.
                _ => current.push('\\'),
            },
            '.' => segments.push(Segment::new(std::mem::take(&mut current))),
            other => current.push(other),
        }
    }
    segments.push(Segment::new(current));
    segments
}

/// Renders segments back into a path string that [`parse`] reads as the same
/// segments.
pub fn join(segments: &[Segment]) -> String {
    let mut out = String::new();
    for (i, segment) in segments.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        for c in segment.key.chars() {
            if c == '.' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out
}

fn child<'a>(value: &'a Value, segment: &Segment) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map.get(segment.key.as_str()),
        Value::Array(items) => items.get(segment.index?),
        _ => None,
    }
}

fn child_mut<'a>(value: &'a mut Value, segment: &Segment) -> Option<&'a mut Value> {
    match value {
        Value::Object(map) => map.get_mut(segment.key.as_str()),
        Value::Array(items) => items.get_mut(segment.index?),
        _ => None,
    }
}

/// Returns the value at `path`, if every step of the path exists. A JSON
/// `null` found at the path is returned as `Some(&Value::Null)`.
pub fn get<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    get_at(root, &parse(path))
}

/// [`get`] for an already parsed path.
pub fn get_at<'a>(root: &'a Value, segments: &[Segment]) -> Option<&'a Value> {
    if segments.is_empty() {
        return None;
    }
    let mut current = root;
    for segment in segments {
        current = child(current, segment)?;
    }
    Some(current)
}

/// Mutable variant of [`get`].
pub fn get_mut<'a>(root: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    get_mut_at(root, &parse(path))
}

/// [`get_mut`] for an already parsed path.
pub fn get_mut_at<'a>(root: &'a mut Value, segments: &[Segment]) -> Option<&'a mut Value> {
    if segments.is_empty() {
        return None;
    }
    let mut current = root;
    for segment in segments {
        current = child_mut(current, segment)?;
    }
    Some(current)
}

/// True when `path` leads to a value that is not JSON `null`.
pub fn exists(root: &Value, path: &str) -> bool {
    exists_at(root, &parse(path))
}

/// [`exists`] for an already parsed path.
pub fn exists_at(root: &Value, segments: &[Segment]) -> bool {
    get_at(root, segments).is_some_and(|v| !v.is_null())
}

/// Writes `value` at `path` and reports whether it was written.
///
/// * Missing intermediate steps are created as **objects** (also when the next
///   segment is numeric: `set({}, "a.0", 1)` gives `{"a":{"0":1}}`). A `null`
///   on the way counts as missing and is replaced by an object.
/// * An existing array is indexed by numeric segments. An index below the
///   length replaces that element, an index equal to the length appends, and
///   anything else (larger index, non-numeric segment) fails.
/// * A scalar (string, number, boolean) on the way is never overwritten.
///
/// A failed call leaves `root` exactly as it was.
pub fn set(root: &mut Value, path: &str, value: Value) -> bool {
    set_at(root, &parse(path), value)
}

/// [`set`] for an already parsed path.
pub fn set_at(root: &mut Value, segments: &[Segment], value: Value) -> bool {
    let Some((last, init)) = segments.split_last() else {
        return false;
    };
    // Decide first, mutate second: a write that fails half-way down must not
    // leave freshly created containers behind.
    if !can_set(root, segments) {
        return false;
    }
    let mut current = root;
    for segment in init {
        if current.is_null() {
            *current = Value::Object(Map::new());
        }
        current = match current {
            Value::Object(map) => map.entry(segment.key.as_str()).or_insert(Value::Null),
            Value::Array(items) => {
                let Some(index) = segment.index else {
                    return false;
                };
                if index == items.len() {
                    items.push(Value::Null);
                }
                match items.get_mut(index) {
                    Some(next) => next,
                    None => return false,
                }
            }
            _ => return false,
        };
    }
    if current.is_null() {
        *current = Value::Object(Map::new());
    }
    match current {
        Value::Object(map) => {
            // With `preserve_order` an existing key keeps its position.
            map.insert(last.key.clone(), value);
            true
        }
        Value::Array(items) => match last.index {
            Some(index) if index < items.len() => {
                items[index] = value;
                true
            }
            Some(index) if index == items.len() => {
                items.push(value);
                true
            }
            _ => false,
        },
        _ => false,
    }
}

/// Whether [`set_at`] can succeed, without touching anything.
fn can_set(root: &Value, segments: &[Segment]) -> bool {
    let mut current = root;
    for segment in segments {
        match current {
            // Becomes an object; everything below is created.
            Value::Null => return true,
            Value::Object(map) => match map.get(segment.key.as_str()) {
                Some(next) => current = next,
                None => return true,
            },
            Value::Array(items) => match segment.index {
                Some(index) if index < items.len() => current = &items[index],
                Some(index) if index == items.len() => return true,
                _ => return false,
            },
            _ => return false,
        }
    }
    // The whole path already exists: its value is replaced.
    true
}

/// Deletes the value at `path`. Returns whether something was deleted.
///
/// Removing an object key keeps the order of the remaining keys; removing an
/// array element shifts the following elements down.
pub fn remove(root: &mut Value, path: &str) -> bool {
    remove_at(root, &parse(path))
}

/// [`remove`] for an already parsed path.
pub fn remove_at(root: &mut Value, segments: &[Segment]) -> bool {
    let Some((last, init)) = segments.split_last() else {
        return false;
    };
    let mut parent = root;
    for segment in init {
        parent = match child_mut(parent, segment) {
            Some(next) => next,
            None => return false,
        };
    }
    match parent {
        // `shift_remove`, not `remove`: bodies are forwarded with their key
        // order intact and the plain remove swaps the last key into the hole.
        Value::Object(map) => map.shift_remove(last.key.as_str()).is_some(),
        Value::Array(items) => match last.index {
            Some(index) if index < items.len() => {
                items.remove(index);
                true
            }
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn keys(path: &str) -> Vec<String> {
        parse(path).iter().map(|s| s.key().to_string()).collect()
    }

    // ----- parse -----------------------------------------------------------

    #[test]
    fn parse_simple_paths() {
        assert_eq!(keys("a"), vec!["a"]);
        assert_eq!(keys("a.b.c"), vec!["a", "b", "c"]);
        assert_eq!(
            keys("generationConfig.thinkingConfig.thinkingBudget"),
            vec!["generationConfig", "thinkingConfig", "thinkingBudget"]
        );
    }

    #[test]
    fn parse_empty_path_has_no_segments() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn parse_empty_segments_are_empty_keys() {
        assert_eq!(keys("."), vec!["", ""]);
        assert_eq!(keys("a..b"), vec!["a", "", "b"]);
        assert_eq!(keys(".a"), vec!["", "a"]);
        assert_eq!(keys("a."), vec!["a", ""]);
    }

    #[test]
    fn parse_escaped_dot() {
        assert_eq!(keys(r"metadata.user\.id"), vec!["metadata", "user.id"]);
        assert_eq!(keys(r"a\.b\.c"), vec!["a.b.c"]);
        assert_eq!(keys(r"\.a"), vec![".a"]);
        assert_eq!(keys(r"a\."), vec!["a."]);
    }

    #[test]
    fn parse_escaped_backslash() {
        assert_eq!(keys(r"a\\.b"), vec![r"a\", "b"]);
        assert_eq!(keys(r"a\\\.b"), vec![r"a\.b"]);
        assert_eq!(keys(r"\\"), vec![r"\"]);
    }

    #[test]
    fn parse_unknown_escape_and_trailing_backslash_are_literal() {
        assert_eq!(keys(r"a\b.c"), vec![r"a\b", "c"]);
        assert_eq!(keys(r"a\"), vec![r"a\"]);
        assert_eq!(keys(r"a\n"), vec![r"a\n"]);
    }

    #[test]
    fn parse_numeric_segments_are_indices() {
        let p = parse("messages.0.role");
        assert_eq!(p[0].index(), None);
        assert_eq!(p[1].index(), Some(0));
        assert_eq!(p[1].key(), "0");
        assert_eq!(parse("a.007")[1].index(), Some(7));
        assert_eq!(parse("a.12")[1].index(), Some(12));
    }

    #[test]
    fn parse_non_canonical_numbers_are_keys() {
        for raw in ["-1", "+1", "1.5", "1e3", " 1", "0x10", "１"] {
            let seg = Segment::new(raw);
            assert_eq!(seg.index(), None, "{raw:?}");
        }
        // Overflowing digits are a key, not a panic.
        let seg = Segment::new("999999999999999999999999999999999999");
        assert_eq!(seg.index(), None);
    }

    #[test]
    fn join_round_trips_through_parse() {
        for path in [
            "a",
            "a.b.c",
            r"metadata.user\.id",
            r"a\\.b",
            "a..b",
            "messages.0.role",
        ] {
            let segments = parse(path);
            assert_eq!(parse(&join(&segments)), segments, "{path}");
        }
        assert_eq!(join(&parse(r"a\b")), r"a\\b");
        assert_eq!(join(&[]), "");
    }

    // ----- get / exists ----------------------------------------------------

    #[test]
    fn get_walks_objects_and_arrays() {
        let v = json!({"a": {"b": [10, {"c": "deep"}]}, "n": null});
        assert_eq!(get(&v, "a.b.0"), Some(&json!(10)));
        assert_eq!(get(&v, "a.b.1.c"), Some(&json!("deep")));
        assert_eq!(get(&v, "a"), Some(&json!({"b": [10, {"c": "deep"}]})));
        assert_eq!(get(&v, "n"), Some(&Value::Null));
    }

    #[test]
    fn get_missing_paths() {
        let v = json!({"a": {"b": [10]}, "s": "text", "n": null});
        assert_eq!(get(&v, "x"), None);
        assert_eq!(get(&v, "a.x"), None);
        assert_eq!(get(&v, "a.b.1"), None);
        assert_eq!(get(&v, "a.b.x"), None);
        assert_eq!(get(&v, "s.len"), None);
        assert_eq!(get(&v, "s.0"), None);
        assert_eq!(get(&v, "n.x"), None);
        assert_eq!(get(&v, ""), None);
    }

    #[test]
    fn get_numeric_key_on_object() {
        let v = json!({"0": "zero", "list": ["first"]});
        assert_eq!(get(&v, "0"), Some(&json!("zero")));
        assert_eq!(get(&v, "list.0"), Some(&json!("first")));
        // "00" is index 0 on an array but the literal key "00" on an object.
        assert_eq!(get(&v, "list.00"), Some(&json!("first")));
        assert_eq!(get(&v, "00"), None);
    }

    #[test]
    fn get_escaped_dot_key() {
        let v = json!({"metadata": {"user.id": "u1", "user": {"id": "nested"}}});
        assert_eq!(get(&v, r"metadata.user\.id"), Some(&json!("u1")));
        assert_eq!(get(&v, "metadata.user.id"), Some(&json!("nested")));
    }

    #[test]
    fn get_empty_key() {
        let v = json!({"": {"": 1}, "a": {"": 2}});
        assert_eq!(get(&v, "."), Some(&json!(1)));
        assert_eq!(get(&v, "a."), Some(&json!(2)));
    }

    #[test]
    fn get_on_scalars_and_arrays_at_root() {
        assert_eq!(get(&json!(5), "a"), None);
        assert_eq!(get(&json!("s"), "0"), None);
        assert_eq!(get(&json!([1, 2]), "1"), Some(&json!(2)));
        assert_eq!(get(&json!([1, 2]), "2"), None);
        assert_eq!(get(&Value::Null, "a"), None);
    }

    #[test]
    fn exists_treats_null_as_absent() {
        let v = json!({"a": null, "b": false, "c": 0, "d": "", "e": {}, "f": []});
        assert!(!exists(&v, "a"));
        assert!(exists(&v, "b"));
        assert!(exists(&v, "c"));
        assert!(exists(&v, "d"));
        assert!(exists(&v, "e"));
        assert!(exists(&v, "f"));
        assert!(!exists(&v, "missing"));
        assert!(!exists(&v, ""));
    }

    #[test]
    fn get_mut_allows_in_place_edits() {
        let mut v = json!({"a": {"b": [1, 2]}});
        *get_mut(&mut v, "a.b.1").unwrap() = json!(20);
        assert_eq!(v, json!({"a": {"b": [1, 20]}}));
        assert!(get_mut(&mut v, "a.b.9").is_none());
        assert!(get_mut(&mut v, "").is_none());
    }

    // ----- set -------------------------------------------------------------

    #[test]
    fn set_replaces_existing_value_in_place() {
        let mut v = json!({"first": 1, "second": 2, "third": 3});
        assert!(set(&mut v, "second", json!("two")));
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"first":1,"second":"two","third":3}"#
        );
    }

    #[test]
    fn set_appends_new_key_at_the_end() {
        let mut v = json!({"model": "m", "stream": true});
        assert!(set(&mut v, "temperature", json!(0.5)));
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"model":"m","stream":true,"temperature":0.5}"#
        );
    }

    #[test]
    fn set_creates_intermediate_objects() {
        let mut v = json!({});
        assert!(set(
            &mut v,
            "generationConfig.thinkingConfig.thinkingBudget",
            json!(32768)
        ));
        assert_eq!(
            v,
            json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 32768}}})
        );
    }

    #[test]
    fn set_numeric_segment_on_missing_container_creates_an_object_key() {
        let mut v = json!({});
        assert!(set(&mut v, "a.0.b", json!(1)));
        assert_eq!(v, json!({"a": {"0": {"b": 1}}}));
    }

    #[test]
    fn set_replaces_null_intermediates_and_null_root() {
        let mut v = json!({"a": null});
        assert!(set(&mut v, "a.b", json!(1)));
        assert_eq!(v, json!({"a": {"b": 1}}));

        let mut v = Value::Null;
        assert!(set(&mut v, "a.b", json!(1)));
        assert_eq!(v, json!({"a": {"b": 1}}));
    }

    #[test]
    fn set_array_element_replace_and_append() {
        let mut v = json!({"list": [1, 2, 3]});
        assert!(set(&mut v, "list.1", json!("two")));
        assert_eq!(v, json!({"list": [1, "two", 3]}));
        assert!(set(&mut v, "list.3", json!(4)));
        assert_eq!(v, json!({"list": [1, "two", 3, 4]}));
    }

    #[test]
    fn set_array_index_beyond_length_is_a_noop() {
        let mut v = json!({"list": [1]});
        let before = v.clone();
        assert!(!set(&mut v, "list.2", json!(9)));
        assert!(!set(&mut v, "list.5.x", json!(9)));
        assert_eq!(v, before);
    }

    #[test]
    fn set_non_numeric_segment_on_array_is_a_noop() {
        let mut v = json!({"list": [1]});
        let before = v.clone();
        assert!(!set(&mut v, "list.name", json!(9)));
        assert!(!set(&mut v, "list.name.x", json!(9)));
        assert!(!set(&mut v, "list.-1", json!(9)));
        assert_eq!(v, before);
    }

    #[test]
    fn set_through_array_appends_an_object_when_index_equals_length() {
        let mut v = json!({"messages": [{"role": "user"}]});
        assert!(set(&mut v, "messages.1.role", json!("assistant")));
        assert_eq!(
            v,
            json!({"messages": [{"role": "user"}, {"role": "assistant"}]})
        );
        assert!(set(&mut v, "messages.0.name", json!("bob")));
        assert_eq!(v["messages"][0], json!({"role": "user", "name": "bob"}));
    }

    #[test]
    fn set_does_not_overwrite_scalars_on_the_way() {
        for scalar in [json!("text"), json!(5), json!(true)] {
            let mut v = json!({"a": scalar});
            let before = v.clone();
            assert!(!set(&mut v, "a.b", json!(1)));
            assert!(!set(&mut v, "a.b.c", json!(1)));
            assert_eq!(v, before);
        }
        let mut v = json!("root scalar");
        assert!(!set(&mut v, "a", json!(1)));
        assert_eq!(v, json!("root scalar"));
    }

    #[test]
    fn set_failure_leaves_no_partial_containers() {
        // The path is fine down to `a.b`, then hits a scalar: nothing along
        // the way may be created or changed.
        let mut v = json!({"a": {"b": "scalar"}, "n": null});
        let before = v.clone();
        assert!(!set(&mut v, "a.b.c.d", json!(1)));
        assert_eq!(v, before);
        let mut v = json!({"n": null, "list": []});
        let before = v.clone();
        assert!(!set(&mut v, "list.1.x", json!(1)));
        assert_eq!(v, before);
    }

    #[test]
    fn set_empty_path_is_a_noop() {
        let mut v = json!({"a": 1});
        assert!(!set(&mut v, "", json!(2)));
        assert_eq!(v, json!({"a": 1}));
    }

    #[test]
    fn set_escaped_dot_key() {
        let mut v = json!({});
        assert!(set(&mut v, r"metadata.user\.id", json!("u")));
        assert_eq!(v, json!({"metadata": {"user.id": "u"}}));
    }

    #[test]
    fn set_root_array() {
        let mut v = json!([1, 2]);
        assert!(set(&mut v, "2", json!(3)));
        assert!(set(&mut v, "0", json!(0)));
        assert!(!set(&mut v, "9", json!(9)));
        assert_eq!(v, json!([0, 2, 3]));
    }

    #[test]
    fn set_can_write_null_and_containers() {
        let mut v = json!({"a": 1});
        assert!(set(&mut v, "a", Value::Null));
        assert!(set(&mut v, "b", json!({"c": [1]})));
        assert_eq!(v, json!({"a": null, "b": {"c": [1]}}));
    }

    // ----- remove ----------------------------------------------------------

    #[test]
    fn remove_object_key_keeps_order_of_the_rest() {
        let mut v = json!({"a": 1, "b": 2, "c": 3, "d": 4});
        assert!(remove(&mut v, "b"));
        assert_eq!(serde_json::to_string(&v).unwrap(), r#"{"a":1,"c":3,"d":4}"#);
    }

    #[test]
    fn remove_nested_and_array_elements() {
        let mut v = json!({"a": {"b": {"c": 1, "d": 2}}, "list": [1, 2, 3]});
        assert!(remove(&mut v, "a.b.c"));
        assert!(remove(&mut v, "list.1"));
        assert_eq!(v, json!({"a": {"b": {"d": 2}}, "list": [1, 3]}));
    }

    #[test]
    fn remove_missing_paths_report_false() {
        let mut v = json!({"a": {"b": 1}, "list": [1], "s": "x"});
        let before = v.clone();
        assert!(!remove(&mut v, "x"));
        assert!(!remove(&mut v, "a.x"));
        assert!(!remove(&mut v, "a.b.c"));
        assert!(!remove(&mut v, "list.5"));
        assert!(!remove(&mut v, "list.name"));
        assert!(!remove(&mut v, "s.0"));
        assert!(!remove(&mut v, ""));
        assert_eq!(v, before);
    }

    #[test]
    fn remove_null_valued_key_counts_as_removed() {
        let mut v = json!({"a": null});
        assert!(remove(&mut v, "a"));
        assert_eq!(v, json!({}));
    }

    #[test]
    fn remove_escaped_dot_key() {
        let mut v = json!({"metadata": {"user.id": 1, "user": {"id": 2}}});
        assert!(remove(&mut v, r"metadata.user\.id"));
        assert_eq!(v, json!({"metadata": {"user": {"id": 2}}}));
    }

    #[test]
    fn remove_leaves_emptied_parents_in_place() {
        let mut v = json!({"reasoning": {"effort": "high"}});
        assert!(remove(&mut v, "reasoning.effort"));
        assert_eq!(v, json!({"reasoning": {}}));
    }

    // ----- robustness ------------------------------------------------------

    #[test]
    fn hostile_inputs_never_panic() {
        let long = "x.".repeat(2000);
        let slashes = "\\".repeat(999);
        let paths = [
            "",
            ".",
            "..",
            "...",
            "\\",
            "\\\\",
            "\\.",
            ".\\",
            "a.\\",
            "0",
            "0.0.0.0",
            "18446744073709551615",
            "18446744073709551616",
            "a.18446744073709551615.b",
            "💥.🔥",
            "a\u{0}b",
            long.as_str(),
            slashes.as_str(),
        ];
        let roots = [
            Value::Null,
            json!(true),
            json!(1.5),
            json!("s"),
            json!([]),
            json!({}),
            json!([[[]]]),
            json!({"a": [{"b": null}], "0": {"0": [0]}, "": {"": ""}}),
        ];
        for root in &roots {
            for path in &paths {
                let _ = get(root, path);
                let _ = exists(root, path);
                let mut copy = root.clone();
                let _ = get_mut(&mut copy, path);
                let _ = set(&mut copy, path, json!(1));
                let mut copy = root.clone();
                let _ = remove(&mut copy, path);
            }
        }
    }

    #[test]
    fn set_then_get_then_remove_round_trip() {
        for path in ["a", "a.b", r"a\.b.c", "x.0.y", "deep.er.and.deeper"] {
            let mut v = json!({});
            assert!(set(&mut v, path, json!("value")), "{path}");
            assert_eq!(get(&v, path), Some(&json!("value")), "{path}");
            assert!(exists(&v, path));
            assert!(remove(&mut v, path), "{path}");
            assert_eq!(get(&v, path), None, "{path}");
        }
    }
}
