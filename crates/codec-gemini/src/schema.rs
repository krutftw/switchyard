//! JSON Schema sanitisation for Gemini.
//!
//! Gemini accepts tool parameter schemas in two fields with different
//! dialects:
//!
//! * `parametersJsonSchema` / `responseJsonSchema` take (a subset of) standard
//!   JSON Schema. [`sanitize_schema`] prepares a schema for these fields: it
//!   repairs the malformed schemas real tool servers produce, inlines local
//!   `$ref`s, flattens `allOf` / `anyOf` / `oneOf` and type unions, and removes
//!   the keywords Gemini rejects, while keeping ordinary constraints
//!   (`pattern`, `minimum`, `additionalProperties`, …).
//! * `parameters` / `responseSchema` take Gemini's OpenAPI-subset `Schema`
//!   message, which rejects every unknown field. [`sanitize_schema_legacy`]
//!   additionally turns the constraints that dialect lacks into description
//!   hints and removes them.
//!
//! Both are pure functions of a **single schema**. They rewrite by key name,
//! so they must never be handed a whole request body.
//!
//! Schemas come from clients, so the cleaner is bounded: inlining `$ref`s and
//! transformation growth share an allowance based on the original schema
//! (see [`INLINE_FACTOR`]). Reference expansion
//! never nests deeper than [`MAX_INLINE_DEPTH`]; references beyond either
//! limit degrade to a `{"type": "object", "description": "See: <name>"}`
//! stub, which is what the reference implementation does with every
//! reference.
//!
//! [`from_gemini_schema`] goes the other way: it turns a Gemini-dialect schema
//! received from a client into standard JSON Schema for the canonical model.

use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

/// How deep a chain of `$ref`s is followed before giving up.
const MAX_REF_DEPTH: usize = 24;
/// Inlining `$ref`s copies definitions, so a schema that references one
/// definition from many places (or nests references) grows. The copies made
/// for one schema may add up to at most this many times the size of the
/// schema itself; references beyond that become `See: <name>` stubs. The
/// cleaned schema is therefore never more than a small multiple of what the
/// client sent.
const INLINE_FACTOR: usize = 4;
/// Absolute ceiling of the same allowance, in (approximate) serialised bytes.
const MAX_INLINE_BYTES: usize = 1 << 20;
/// A definition is only inlined where the result stays within this many
/// levels of JSON nesting. Chained references would otherwise multiply the
/// depth of the schema, past what JSON parsers (and this crate's own
/// recursive passes) accept.
const MAX_INLINE_DEPTH: usize = 64;

/// Description of the placeholder property some gateways inject into
/// parameter-less tools; it is removed again here.
const PLACEHOLDER_REASON: &str = "Brief explanation of why you are calling this tool";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Target is `parametersJsonSchema` / `responseJsonSchema`.
    JsonSchema,
    /// Target is the OpenAPI-subset `parameters` / `responseSchema`.
    Legacy,
}

/// Cleans a JSON Schema for Gemini's `parametersJsonSchema` field.
///
/// The result is idempotent: `sanitize_schema(&sanitize_schema(x))` equals
/// `sanitize_schema(x)`. It is also bounded: local `$ref`s are inlined only
/// while the copies stay within a few times the size of `schema` and within
/// 64 levels of nesting; the references left over become
/// `{"type": "object", "description": "See: <name>"}` stubs, like remote,
/// dangling and recursive ones.
pub fn sanitize_schema(schema: &Value) -> Value {
    sanitize(schema, Mode::JsonSchema)
}

/// Cleans a JSON Schema for Gemini's restricted `parameters` /
/// `responseSchema` dialect. On top of [`sanitize_schema`] it:
///
/// * turns `additionalProperties: false` into the description hint
///   `No extra properties allowed` and removes `additionalProperties`;
/// * moves `minLength`, `maxLength`, `exclusiveMinimum`, `exclusiveMaximum`,
///   `pattern`, `minItems`, `maxItems`, `uniqueItems`, `contains`, `default`,
///   `examples`, `multipleOf` and unsupported `format` values into
///   `<keyword>: <value>` description hints and removes them (`format` is kept
///   when it is one the dialect knows: `enum` / `date-time` on strings,
///   `int32` / `int64` on integers, `float` / `double` on numbers);
/// * removes every remaining keyword the `Schema` message does not define.
pub fn sanitize_schema_legacy(schema: &Value) -> Value {
    sanitize(schema, Mode::Legacy)
}

fn sanitize(schema: &Value, mode: Mode) -> Value {
    let map = match schema {
        // `true` is the schema that accepts anything.
        Value::Bool(true) => return Value::Object(Map::new()),
        Value::Object(map) => map,
        other => return other.clone(),
    };
    if looks_like_request(map) {
        // Rewriting by key name would corrupt tool-call arguments in history.
        return schema.clone();
    }
    // Some tool servers wrap the schema: `{"schema": {...}}`.
    if map.len() == 1
        && let Some(inner) = map.get("schema")
        && (inner.is_object() || inner.is_boolean())
    {
        let mut wrapper = Map::new();
        wrapper.insert("schema".to_string(), sanitize(inner, mode));
        return Value::Object(wrapper);
    }

    let mut root = schema.clone();
    let (size, _) = measure(schema);
    let mut ctx = RefCtx {
        root: schema,
        stack: Vec::new(),
        budget: size.saturating_mul(INLINE_FACTOR).min(MAX_INLINE_BYTES),
        measured: HashMap::new(),
    };
    prepare(&mut root, &mut ctx, 0);
    transform_bounded(&mut root, mode, &mut ctx.budget);
    prune(&mut root, mode);
    root
}

/// A body that is an API request rather than a schema.
fn looks_like_request(map: &Map<String, Value>) -> bool {
    const REQUEST_ARRAYS: [&str; 5] = [
        "tools",
        "contents",
        "messages",
        "functionDeclarations",
        "function_declarations",
    ];
    let has_arrays = |m: &Map<String, Value>| {
        REQUEST_ARRAYS
            .iter()
            .any(|k| m.get(*k).is_some_and(Value::is_array))
    };
    if has_arrays(map) {
        return true;
    }
    matches!(map.get("request"), Some(Value::Object(inner)) if has_arrays(inner))
}

// ---------------------------------------------------------------------------
// Pass A: inline `$ref`, repair malformed schemas (top-down)
// ---------------------------------------------------------------------------

struct RefCtx<'a> {
    /// The schema as the caller passed it; `$ref` pointers resolve against it.
    root: &'a Value,
    /// References being expanded on the current path, to detect recursion.
    stack: Vec<*const Value>,
    /// What is left of the inlining allowance, in the unit of [`measure`].
    budget: usize,
    /// Size and nesting depth of the definitions looked at so far, by
    /// resolved node identity, so aliases of a definition are measured once.
    measured: HashMap<*const Value, (usize, usize)>,
}

impl<'a> RefCtx<'a> {
    /// Resolves a local reference (`#/$defs/Name`, `#/definitions/Name`, `#`).
    fn resolve(&self, reference: &str) -> Option<&'a Value> {
        let pointer = reference.strip_prefix('#')?;
        if pointer.is_empty() {
            return Some(self.root);
        }
        let mut node = self.root;
        for raw in pointer.strip_prefix('/')?.split('/') {
            let segment = raw.replace("~1", "/").replace("~0", "~");
            node = match node {
                Value::Object(map) => map.get(&segment)?,
                Value::Array(list) => list.get(segment.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(node)
    }

    /// The definition `reference` points at, if it may be copied to a node
    /// that sits `depth` levels deep: it must exist, must not be on the
    /// current expansion path, must fit in what is left of the size allowance
    /// and must not push the schema past [`MAX_INLINE_DEPTH`]. The allowance
    /// is charged here.
    fn take(&mut self, reference: &str, depth: usize) -> Option<Map<String, Value>> {
        if self.stack.len() >= MAX_REF_DEPTH || self.budget == 0 {
            return None;
        }
        let target = self.resolve(reference)?;
        let identity = std::ptr::from_ref(target);
        if self.stack.contains(&identity) {
            return None;
        }
        let (size, levels) = match self.measured.get(&identity) {
            Some(known) => *known,
            None => {
                let fresh = measure(target);
                self.measured.insert(identity, fresh);
                fresh
            }
        };
        if size > self.budget || depth.saturating_add(levels) > MAX_INLINE_DEPTH {
            return None;
        }
        let copy = match target {
            Value::Object(map) => map.clone(),
            Value::Bool(true) => Map::new(),
            _ => return None,
        };
        self.budget -= size;
        Some(copy)
    }
}

/// Approximate serialised size of `value` in bytes and the number of nested
/// containers at its deepest point (`0` for a scalar, `1` for `{}`). Walks
/// with an explicit stack, so it is safe on any input.
fn measure(value: &Value) -> (usize, usize) {
    let mut size = 0usize;
    let mut deepest = 0usize;
    let mut pending: Vec<(&Value, usize)> = vec![(value, 0)];
    while let Some((node, depth)) = pending.pop() {
        match node {
            Value::Null | Value::Bool(_) => size += 4,
            Value::Number(_) => size += 8,
            Value::String(text) => size += text.len() + 2,
            Value::Array(list) => {
                deepest = deepest.max(depth + 1);
                size += 2 + list.len();
                pending.extend(list.iter().map(|child| (child, depth + 1)));
            }
            Value::Object(map) => {
                deepest = deepest.max(depth + 1);
                size += 2 + map.len();
                for (key, child) in map {
                    size += key.len() + 3;
                    pending.push((child, depth + 1));
                }
            }
        }
    }
    (size, deepest)
}

/// Last path segment of a reference, for the `See: <name>` stub.
fn ref_name(reference: &str) -> String {
    let last = reference.rsplit('/').next().unwrap_or(reference);
    let name = last.replace("~1", "/").replace("~0", "~");
    let name = name.trim_start_matches('#');
    if name.is_empty() {
        "#".to_string()
    } else {
        name.to_string()
    }
}

/// `depth` is the number of JSON containers around `value` in the schema
/// being built.
fn prepare(value: &mut Value, ctx: &mut RefCtx<'_>, depth: usize) {
    if matches!(value, Value::Bool(true)) {
        *value = Value::Object(Map::new());
        return;
    }
    if !value.is_object() {
        return;
    }
    let pushed = inline_ref(value, ctx, depth);
    if let Value::Object(map) = value {
        repair(map);
        for (key, child) in map.iter_mut() {
            match key.as_str() {
                "properties" | "dependentSchemas" => {
                    if let Value::Object(named) = child {
                        for (_, schema) in named.iter_mut() {
                            prepare(schema, ctx, depth + 2);
                        }
                    }
                }
                "dependencies" => {
                    if let Value::Object(named) = child {
                        for (_, schema) in named.iter_mut() {
                            // Array values are the `dependentRequired` form.
                            if !schema.is_array() {
                                prepare(schema, ctx, depth + 2);
                            }
                        }
                    }
                }
                "items" => match child {
                    Value::Array(tuple) => {
                        tuple.iter_mut().for_each(|s| prepare(s, ctx, depth + 2))
                    }
                    single => prepare(single, ctx, depth + 1),
                },
                // `additionalProperties: true` is a flag, not a schema to
                // normalise.
                "additionalProperties" if child.is_object() => prepare(child, ctx, depth + 1),
                "not" | "contains" | "then" | "else" => prepare(child, ctx, depth + 1),
                "anyOf" | "oneOf" | "allOf" | "prefixItems" => {
                    if let Value::Array(list) = child {
                        list.iter_mut().for_each(|s| prepare(s, ctx, depth + 2));
                    }
                }
                _ => {}
            }
        }
        // After the children: a property that was a `$ref` has its real
        // shape only now.
        promote_required(map);
    }
    for _ in 0..pushed {
        ctx.stack.pop();
    }
}

/// Replaces a node that is a `$ref` by the definition it points at (sibling
/// keywords of the reference win over the definition's). References that are
/// remote, dangling, recursive or too deep, and references that would make
/// the schema grow past its allowance (see [`INLINE_FACTOR`]), become a
/// `See: <name>` stub. Returns how many references were pushed on the
/// recursion stack.
fn inline_ref(value: &mut Value, ctx: &mut RefCtx<'_>, depth: usize) -> usize {
    let mut pushed = 0;
    loop {
        let Some(reference) = value.get("$ref").and_then(Value::as_str).map(str::to_owned) else {
            // A non-string `$ref` is junk; pass C deletes it.
            return pushed;
        };
        let target = ctx.take(&reference, depth);
        let siblings = match std::mem::take(value) {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        match target {
            Some(mut merged) => {
                for (key, sibling) in siblings {
                    if key != "$ref" {
                        merged.insert(key, sibling);
                    }
                }
                *value = Value::Object(merged);
                // The immutable source tree keeps node identities stable.
                ctx.stack
                    .push(std::ptr::from_ref(ctx.resolve(&reference).unwrap()));
                pushed += 1;
            }
            None => {
                let hint = format!("See: {}", ref_name(&reference));
                let description = match siblings.get("description").and_then(Value::as_str) {
                    Some(existing) if !existing.is_empty() => format!("{existing} ({hint})"),
                    _ => hint,
                };
                let mut stub = Map::new();
                stub.insert("type".to_string(), Value::String("object".to_string()));
                stub.insert("description".to_string(), Value::String(description));
                *value = Value::Object(stub);
                return pushed;
            }
        }
    }
}

/// Keywords whose value may be an object without being a property definition.
fn is_known_keyword(key: &str) -> bool {
    key.starts_with("x-")
        || matches!(
            key,
            "properties"
                | "patternProperties"
                | "additionalProperties"
                | "items"
                | "prefixItems"
                | "$defs"
                | "definitions"
                | "$vocabulary"
                | "dependentSchemas"
                | "dependentRequired"
                | "dependencies"
                | "if"
                | "then"
                | "else"
                | "not"
                | "contains"
                | "propertyNames"
                | "unevaluatedProperties"
                | "unevaluatedItems"
                | "contentSchema"
                | "additionalItems"
                | "default"
                | "const"
                | "example"
                | "examples"
                | "discriminator"
                | "xml"
                | "externalDocs"
                | "enumDescriptions"
                | "enumTitles"
        )
}

fn type_contains(map: &Map<String, Value>, wanted: &str) -> bool {
    match map.get("type") {
        Some(Value::String(t)) => t.eq_ignore_ascii_case(wanted),
        Some(Value::Array(list)) => list
            .iter()
            .any(|t| t.as_str().is_some_and(|t| t.eq_ignore_ascii_case(wanted))),
        _ => false,
    }
}

fn type_is_missing(map: &Map<String, Value>) -> bool {
    match map.get("type") {
        None | Some(Value::Null) => true,
        Some(Value::String(t)) => t.is_empty(),
        _ => false,
    }
}

/// Whether a node is (or may be) an object schema: its `type` is absent,
/// `object`, or a union containing `object`. An object-valued `type` is not a
/// type but a property called "type".
fn is_object_like(map: &Map<String, Value>) -> bool {
    match map.get("type") {
        None | Some(Value::Null) | Some(Value::Object(_)) => true,
        Some(Value::String(t)) => t.is_empty() || t.eq_ignore_ascii_case("object"),
        Some(Value::Array(_)) => type_contains(map, "object"),
        _ => false,
    }
}

/// Phase 0 of the notes: fixes the schema shapes broken tool servers emit.
fn repair(map: &mut Map<String, Value>) {
    // Bare property maps: `{"path": {"type": "string"}}` with no `properties`.
    let is_stray = |key: &str, value: &Value| value.is_object() && !is_known_keyword(key);
    if is_object_like(map) && map.iter().any(|(key, value)| is_stray(key, value)) {
        // One pass over the node, however many stray keys it has.
        let mut moved = Vec::new();
        for (key, value) in std::mem::take(map) {
            if is_stray(&key, &value) {
                moved.push((key, value));
            } else {
                map.insert(key, value);
            }
        }
        let had_properties = matches!(map.get("properties"), Some(Value::Object(_)));
        if !had_properties {
            if type_is_missing(map) {
                map.insert("type".to_string(), Value::String("object".to_string()));
            }
            map.insert("properties".to_string(), Value::Object(Map::new()));
        }
        if let Some(Value::Object(properties)) = map.get_mut("properties") {
            for (key, value) in moved {
                properties.entry(key).or_insert(value);
            }
        }
    }

    // Inside a property map `true` means "anything".
    if let Some(Value::Object(properties)) = map.get_mut("properties") {
        for (_, property) in properties.iter_mut() {
            if matches!(property, Value::Bool(true)) {
                *property = Value::Object(Map::new());
            }
        }
    }

    // Arrays need `items`; `items` implies an array.
    let has_items = map.contains_key("items");
    if type_contains(map, "array") && !has_items {
        let mut items = Map::new();
        items.insert("type".to_string(), Value::String("string".to_string()));
        map.insert("items".to_string(), Value::Object(items));
    } else if has_items && type_is_missing(map) {
        map.insert("type".to_string(), Value::String("array".to_string()));
    }
}

/// A boolean `required` on a property (the draft-03 spelling) belongs in the
/// parent's `required` list. Runs once the properties are fully prepared, so
/// that a property defined through a `$ref` is seen as what it refers to.
fn promote_required(map: &mut Map<String, Value>) {
    let mut promoted: Vec<String> = Vec::new();
    if let Some(Value::Object(properties)) = map.get_mut("properties") {
        for (name, property) in properties.iter_mut() {
            if let Value::Object(schema) = property
                && let Some(Value::Bool(flag)) = schema.get("required")
            {
                let flag = *flag;
                schema.shift_remove("required");
                if flag {
                    promoted.push(name.clone());
                }
            }
        }
    }
    if promoted.is_empty() {
        return;
    }
    promoted.sort();
    let mut required = match map.get("required") {
        Some(Value::Array(existing)) => existing.clone(),
        _ => Vec::new(),
    };
    let mut listed = string_set(&required);
    for name in promoted {
        if listed.insert(name.clone()) {
            required.push(Value::String(name));
        }
    }
    map.insert("required".to_string(), Value::Array(required));
}

/// The strings of a JSON list, for membership tests that stay cheap on lists
/// of any length.
fn string_set(list: &[Value]) -> HashSet<String> {
    list.iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

// ---------------------------------------------------------------------------
// Pass B: convert and flatten (bottom-up)
// ---------------------------------------------------------------------------

/// Appends `hint` to the node's description. Idempotent, because a schema may
/// be cleaned more than once.
fn append_hint(map: &mut Map<String, Value>, hint: &str) {
    let existing = map.get("description").and_then(Value::as_str).unwrap_or("");
    let updated = if existing.is_empty() {
        hint.to_string()
    } else if existing == hint
        || existing.starts_with(&format!("{hint} ("))
        || existing.contains(&format!("({hint})"))
    {
        return;
    } else {
        format!("{existing} ({hint})")
    };
    map.insert("description".to_string(), Value::String(updated));
}

/// Marks a property as nullable in its description.
fn append_nullable(map: &mut Map<String, Value>) {
    let existing = map.get("description").and_then(Value::as_str).unwrap_or("");
    let updated = if existing.is_empty() {
        "(nullable)".to_string()
    } else if existing.contains("(nullable)") {
        return;
    } else {
        format!("{existing} (nullable)")
    };
    map.insert("description".to_string(), Value::String(updated));
}

/// Text of a JSON value inside a description hint or a stringified enum.
fn plain_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
fn transform(value: &mut Value, mode: Mode) {
    let mut budget = MAX_INLINE_BYTES;
    transform_bounded(value, mode, &mut budget);
}

/// Growth is charged after each local rewrite, before an ancestor can copy
/// or stringify its output. Local rewrites perform a fixed number of merges
/// and serializations; an oversized result cannot multiply at later levels.
fn transform_bounded(value: &mut Value, mode: Mode, budget: &mut usize) {
    let Value::Object(map) = value else {
        return;
    };
    for (key, child) in map.iter_mut() {
        match key.as_str() {
            "properties" | "dependentSchemas" | "dependencies" => {
                if let Value::Object(named) = child {
                    named
                        .iter_mut()
                        .for_each(|(_, schema)| transform_bounded(schema, mode, budget));
                }
            }
            "items" => match child {
                Value::Array(tuple) => tuple
                    .iter_mut()
                    .for_each(|s| transform_bounded(s, mode, budget)),
                single => transform_bounded(single, mode, budget),
            },
            "additionalProperties" | "not" | "contains" | "then" | "else" => {
                transform_bounded(child, mode, budget)
            }
            "anyOf" | "oneOf" | "allOf" | "prefixItems" => {
                if let Value::Array(list) = child {
                    list.iter_mut()
                        .for_each(|s| transform_bounded(s, mode, budget));
                }
            }
            _ => {}
        }
    }
    let before = measure_map(map);
    convert(map, mode);
    flatten(map);
    settle_enum(map);
    settle_nullable_properties(map);
    let growth = measure_map(map).saturating_sub(before);
    if growth > *budget {
        map.clear();
        map.insert("type".to_string(), Value::String("object".to_string()));
        map.insert(
            "description".to_string(),
            Value::String("Schema omitted: transformation budget exceeded".to_string()),
        );
        *budget = 0;
    } else {
        *budget -= growth;
    }
}

fn measure_map(map: &Map<String, Value>) -> usize {
    map.iter().fold(2 + map.len(), |size, (key, value)| {
        size.saturating_add(key.len() + 3)
            .saturating_add(measure(value).0)
    })
}

/// Flattening can put an `enum` and a foreign `type` on the same node: the
/// node's own enum next to the type of the union branch that replaced it, or
/// an enum merged in from an `allOf` member next to the node's own type.
/// Gemini only accepts string enums, so the invariant "a node with an `enum`
/// is a string whose values are strings" is re-established once the node has
/// its final shape.
fn settle_enum(map: &mut Map<String, Value>) {
    let Some(Value::Array(values)) = map.get_mut("enum") else {
        return;
    };
    for value in values.iter_mut() {
        if !value.is_string() {
            *value = Value::String(plain_text(value));
        }
    }
    // An enum that arrived through a merge has not been hinted yet.
    let hint = (2..=10).contains(&values.len()).then(|| {
        let listed: Vec<&str> = values.iter().filter_map(Value::as_str).collect();
        format!("Allowed: {}", listed.join(", "))
    });
    map.insert("type".to_string(), Value::String("string".to_string()));
    if let Some(hint) = hint {
        append_hint(map, &hint);
    }
}

/// Hint keywords of the legacy dialect, in the order hints are appended.
const LEGACY_HINTED: [&str; 13] = [
    "minLength",
    "maxLength",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "pattern",
    "minItems",
    "maxItems",
    "uniqueItems",
    "contains",
    "format",
    "default",
    "examples",
    "multipleOf",
];

/// Whether the legacy dialect understands `format` on a node of this type.
fn legacy_format_supported(map: &Map<String, Value>) -> bool {
    let Some(format) = map.get("format").and_then(Value::as_str) else {
        return false;
    };
    let ty = map
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        (ty.as_str(), format),
        ("string", "enum" | "date-time")
            | ("integer", "int32" | "int64")
            | ("number", "float" | "double")
    )
}

/// Phase 1 of the notes: `const`, `enum` and description hints.
fn convert(map: &mut Map<String, Value>, mode: Mode) {
    if !map.contains_key("enum")
        && let Some(constant) = map.get("const").cloned()
    {
        map.insert("enum".to_string(), Value::Array(vec![constant]));
    }

    // Gemini only accepts string enums.
    let mut enum_hint = None;
    let mut is_enum = false;
    if let Some(Value::Array(values)) = map.get_mut("enum") {
        is_enum = true;
        for value in values.iter_mut() {
            if !value.is_string() {
                *value = Value::String(plain_text(value));
            }
        }
        if (2..=10).contains(&values.len()) {
            let listed: Vec<&str> = values.iter().filter_map(Value::as_str).collect();
            enum_hint = Some(format!("Allowed: {}", listed.join(", ")));
        }
    }
    if is_enum {
        if type_contains(map, "null") && map.get("type").is_some_and(Value::is_array) {
            map.insert("nullable".to_string(), Value::Bool(true));
        }
        map.insert("type".to_string(), Value::String("string".to_string()));
    }
    if let Some(hint) = enum_hint {
        append_hint(map, &hint);
    }

    if mode == Mode::Legacy {
        if map.get("additionalProperties") == Some(&Value::Bool(false)) {
            append_hint(map, "No extra properties allowed");
        }
        for key in LEGACY_HINTED {
            if key == "format" && legacy_format_supported(map) {
                continue;
            }
            if let Some(value) = map.get(key) {
                let hint = format!("{key}: {}", plain_text(value));
                append_hint(map, &hint);
            }
        }
    }
}

/// Adds `extra` to the node's `properties` (created when missing) without
/// replacing properties the node already defines.
fn add_properties(map: &mut Map<String, Value>, extra: &Map<String, Value>) {
    if !matches!(map.get("properties"), Some(Value::Object(_))) {
        map.insert("properties".to_string(), Value::Object(Map::new()));
    }
    if let Some(Value::Object(properties)) = map.get_mut("properties") {
        for (name, schema) in extra {
            if !properties.contains_key(name) {
                properties.insert(name.clone(), schema.clone());
            }
        }
    }
}

/// Merges the schema `source` into the schema `target` without replacing
/// anything `target` already defines. Where both define the same property (or
/// the same schema-valued keyword) the two sub-schemas are merged the same
/// way; data values such as `default` are never mixed.
///
/// Both sides have been converted and flattened already, so a sub-schema that
/// gains keywords here is brought back to the string-enum invariant on the
/// spot (the caller does that for `target` itself).
fn fill_missing(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    fn merge_schema(own: &mut Value, other: &Value) {
        if let (Value::Object(own), Value::Object(other)) = (own, other) {
            fill_missing(own, other);
            settle_enum(own);
        }
    }
    for (key, value) in source {
        let Some(existing) = target.get_mut(key) else {
            target.insert(key.clone(), value.clone());
            continue;
        };
        match key.as_str() {
            "properties" | "dependentSchemas" => {
                if let (Value::Object(named), Value::Object(incoming)) = (existing, value) {
                    for (name, schema) in incoming {
                        match named.get_mut(name) {
                            None => {
                                named.insert(name.clone(), schema.clone());
                            }
                            Some(own) => merge_schema(own, schema),
                        }
                    }
                }
            }
            "items" | "additionalProperties" | "not" | "contains" => merge_schema(existing, value),
            _ => {}
        }
    }
}

fn is_null_branch(branch: &Value) -> bool {
    branch
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.eq_ignore_ascii_case("null"))
}

/// The type a union branch stands for, for scoring and the `Accepts:` hint.
fn branch_type(branch: &Value) -> Option<String> {
    let map = branch.as_object()?;
    match map.get("type").and_then(Value::as_str) {
        Some(t) if t.eq_ignore_ascii_case("null") => None,
        Some(t) if !t.is_empty() => Some(t.to_string()),
        _ if map.contains_key("properties") => Some("object".to_string()),
        _ if map.contains_key("items") => Some("array".to_string()),
        _ => None,
    }
}

fn branch_score(branch: &Value) -> u8 {
    match branch_type(branch) {
        Some(t) if t.eq_ignore_ascii_case("object") => 3,
        Some(t) if t.eq_ignore_ascii_case("array") => 2,
        Some(_) => 1,
        None => 0,
    }
}

/// Phase 2 of the notes: `then`/`else`, `allOf`, `anyOf`/`oneOf`, type arrays.
fn flatten(map: &mut Map<String, Value>) {
    // Conditional branches contribute their properties.
    for branch in ["then", "else"] {
        // Consume the branch before merging: keeping it here duplicates all
        // already transformed descendants until the later prune pass.
        let Some(Value::Object(mut branch)) = map.shift_remove(branch) else {
            continue;
        };
        let Some(Value::Object(extra)) = branch.shift_remove("properties") else {
            continue;
        };
        add_properties(map, &extra);
    }

    if let Some(Value::Array(members)) = map.shift_remove("allOf") {
        // The union of the `required` lists, collected across all members and
        // written back once.
        let mut required: Option<(Vec<Value>, HashSet<String>)> = None;
        for member in members {
            let Value::Object(mut member) = member else {
                continue;
            };
            for skipped in ["if", "then", "else", "allOf"] {
                member.shift_remove(skipped);
            }
            if let Some(Value::Array(extra)) = member.shift_remove("required") {
                let (names, listed) = required.get_or_insert_with(|| {
                    let existing = match map.get("required") {
                        Some(Value::Array(existing)) => existing.clone(),
                        _ => Vec::new(),
                    };
                    let listed = string_set(&existing);
                    (existing, listed)
                });
                for name in extra {
                    // Anything but a string is not a property name.
                    if let Some(text) = name.as_str()
                        && listed.insert(text.to_string())
                    {
                        names.push(name);
                    }
                }
            }
            fill_missing(map, &member);
        }
        if let Some((names, _)) = required {
            map.insert("required".to_string(), Value::Array(names));
        }
    }

    for union in ["anyOf", "oneOf"] {
        let branches = match map.get(union) {
            Some(Value::Array(branches)) if !branches.is_empty() => branches.clone(),
            _ => continue,
        };
        map.shift_remove(union);
        let has_null = branches.iter().any(is_null_branch);

        if matches!(map.get("properties"), Some(Value::Object(_))) {
            // The parent is already an object: branches only add properties.
            for branch in &branches {
                if let Some(Value::Object(extra)) = branch.get("properties") {
                    add_properties(map, extra);
                }
            }
            if has_null {
                map.insert("nullable".to_string(), Value::Bool(true));
            }
            continue;
        }

        // Otherwise the most structured branch stands in for the union.
        let mut best = 0;
        for (index, branch) in branches.iter().enumerate() {
            if branch_score(branch) > branch_score(&branches[best]) {
                best = index;
            }
        }
        let mut chosen = match &branches[best] {
            Value::Object(branch) => branch.clone(),
            _ => Map::new(),
        };
        let parent_description = map
            .get("description")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .map(str::to_owned);
        if let Some(parent) = parent_description {
            let merged = match chosen.get("description").and_then(Value::as_str) {
                Some(child) if !child.is_empty() && child != parent => {
                    format!("{parent} ({child})")
                }
                _ => parent,
            };
            chosen.insert("description".to_string(), Value::String(merged));
        }
        // Keywords the parent set next to the union (`default`, `required`,
        // a second union, …) survive unless the branch defines them itself.
        for (key, value) in map.iter() {
            if key != "description" && !chosen.contains_key(key) {
                chosen.insert(key.clone(), value.clone());
            }
        }
        if has_null && !is_null_branch(&branches[best]) {
            chosen.insert("nullable".to_string(), Value::Bool(true));
        }
        let mut types: Vec<String> = Vec::new();
        let mut listed: HashSet<String> = HashSet::new();
        for branch in &branches {
            if let Some(t) = branch_type(branch)
                && listed.insert(t.clone())
            {
                types.push(t);
            }
        }
        if types.len() > 1 {
            append_hint(&mut chosen, &format!("Accepts: {}", types.join(" | ")));
        }
        *map = chosen;
    }

    // `"type": ["string", "null"]` becomes a single type.
    if let Some(Value::Array(types)) = map.get("type") {
        if types.is_empty() {
            map.shift_remove("type");
        } else {
            let names: Vec<String> = types
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            let has_null = names.iter().any(|t| t.eq_ignore_ascii_case("null"));
            let concrete: Vec<&String> = names
                .iter()
                .filter(|t| !t.eq_ignore_ascii_case("null"))
                .collect();
            let has_items = map.contains_key("items");
            let chosen = if has_items && concrete.iter().any(|t| t.eq_ignore_ascii_case("array")) {
                "array".to_string()
            } else {
                concrete
                    .first()
                    .map(|t| (*t).clone())
                    .unwrap_or_else(|| "string".to_string())
            };
            if has_items && !chosen.eq_ignore_ascii_case("array") {
                map.shift_remove("items");
            }
            let hint = (concrete.len() > 1).then(|| {
                let listed: Vec<&str> = concrete.iter().map(|t| t.as_str()).collect();
                format!("Accepts: {}", listed.join(" | "))
            });
            map.insert("type".to_string(), Value::String(chosen));
            if let Some(hint) = hint {
                append_hint(map, &hint);
            }
            if has_null {
                map.insert("nullable".to_string(), Value::Bool(true));
            }
        }
    }
}

/// A property that may be `null` cannot be mandatory once its type has been
/// narrowed: say so in its description and drop it from `required`.
fn settle_nullable_properties(map: &mut Map<String, Value>) {
    let mut nullable: HashSet<String> = HashSet::new();
    if let Some(Value::Object(properties)) = map.get_mut("properties") {
        for (name, property) in properties.iter_mut() {
            if let Value::Object(schema) = property
                && schema.get("nullable") == Some(&Value::Bool(true))
            {
                append_nullable(schema);
                nullable.insert(name.clone());
            }
        }
    }
    if nullable.is_empty() {
        return;
    }
    if let Some(Value::Array(required)) = map.get_mut("required") {
        required.retain(|name| !name.as_str().is_some_and(|n| nullable.contains(n)));
        if required.is_empty() {
            map.shift_remove("required");
        }
    }
}

// ---------------------------------------------------------------------------
// Pass C: delete what Gemini does not accept
// ---------------------------------------------------------------------------

/// Keywords removed in both dialects.
fn is_deleted_keyword(key: &str) -> bool {
    key.starts_with("x-")
        || matches!(
            key,
            "$schema"
                | "$defs"
                | "definitions"
                | "const"
                | "$ref"
                | "$id"
                | "id"
                | "$anchor"
                | "$vocabulary"
                | "$dynamicRef"
                | "$dynamicAnchor"
                | "propertyNames"
                | "patternProperties"
                | "if"
                | "then"
                | "else"
                | "$comment"
                | "enumDescriptions"
                | "enumTitles"
                | "prefill"
                | "deprecated"
                | "encrypted"
                | "additionalItems"
                | "unevaluatedProperties"
                | "unevaluatedItems"
                | "contentSchema"
                | "nullable"
                | "title"
        )
}

/// Fields of Gemini's `Schema` message that survive in the legacy dialect.
fn is_legacy_keyword(key: &str) -> bool {
    matches!(
        key,
        "type"
            | "format"
            | "description"
            | "enum"
            | "items"
            | "properties"
            | "required"
            | "minimum"
            | "maximum"
            | "minProperties"
            | "maxProperties"
            | "propertyOrdering"
            | "example"
    )
}

fn prune(value: &mut Value, mode: Mode) {
    let Value::Object(map) = value else {
        return;
    };
    let keep_format = mode == Mode::Legacy && legacy_format_supported(map);
    map.retain(|key, _| {
        if is_deleted_keyword(key) {
            return false;
        }
        match mode {
            Mode::JsonSchema => true,
            Mode::Legacy => is_legacy_keyword(key) && (key != "format" || keep_format),
        }
    });

    // Bare property maps were moved under `properties` in pass A, for nodes
    // that were objects then. A node that became an object only while it was
    // flattened (a union branch took its place, an `allOf` member was merged
    // in) can still carry object-valued keys that are not keywords. They were
    // never cleaned and would be read as properties by the next reader of
    // this schema, so they go.
    if mode == Mode::JsonSchema && is_object_like(map) {
        map.retain(|key, value| !value.is_object() || is_known_keyword(key));
    }

    // Placeholder properties injected by other gateways for parameter-less
    // tools.
    let mut dropped: Vec<&str> = Vec::new();
    if let Some(Value::Object(properties)) = map.get_mut("properties") {
        if properties.shift_remove("_").is_some() {
            dropped.push("_");
        }
        let only_reason = properties.len() == 1
            && properties
                .get("reason")
                .and_then(|r| r.get("description"))
                .and_then(Value::as_str)
                == Some(PLACEHOLDER_REASON);
        if only_reason {
            properties.shift_remove("reason");
            dropped.push("reason");
        }
    }
    if !dropped.is_empty()
        && let Some(Value::Array(required)) = map.get_mut("required")
    {
        required.retain(|name| !name.as_str().is_some_and(|n| dropped.contains(&n)));
    }

    // `required` is a list of names. A boolean that was not promoted to a
    // parent (the node is not a property) has nowhere to go.
    if map.get("required").is_some_and(|r| !r.is_array()) {
        map.shift_remove("required");
    }
    // `required` may only name properties that exist.
    if let Some(Value::Array(required)) = map.get("required") {
        let kept: Vec<Value> = match map.get("properties") {
            Some(Value::Object(properties)) => required
                .iter()
                .filter(|name| name.as_str().is_some_and(|n| properties.contains_key(n)))
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        if kept.is_empty() {
            map.shift_remove("required");
        } else {
            map.insert("required".to_string(), Value::Array(kept));
        }
    }

    // `items` only makes sense on arrays.
    if map.contains_key("items") {
        if type_is_missing(map) {
            map.insert("type".to_string(), Value::String("array".to_string()));
        } else if !type_contains(map, "array") {
            map.shift_remove("items");
        }
    }

    for (key, child) in map.iter_mut() {
        match key.as_str() {
            "properties" | "dependentSchemas" | "dependencies" => {
                if let Value::Object(named) = child {
                    named.iter_mut().for_each(|(_, schema)| prune(schema, mode));
                }
            }
            "items" => match child {
                Value::Array(tuple) => tuple.iter_mut().for_each(|s| prune(s, mode)),
                single => prune(single, mode),
            },
            "additionalProperties" | "not" | "contains" => prune(child, mode),
            "anyOf" | "oneOf" | "prefixItems" => {
                if let Value::Array(list) = child {
                    list.iter_mut().for_each(|s| prune(s, mode));
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Gemini dialect -> JSON Schema
// ---------------------------------------------------------------------------

/// Converts a schema written in Gemini's OpenAPI-subset dialect (`parameters`,
/// `responseSchema`) into standard JSON Schema: upper-case type names
/// (`OBJECT`, `STRING`) are lower-cased, `nullable: true` becomes a
/// `["<type>", "null"]` union and the Gemini-only `propertyOrdering` is
/// dropped. Numeric constraints written as strings become numbers: the
/// dialect is a protobuf message, whose `int64` fields (`minItems`,
/// `maxLength`, …) are rendered as JSON strings and whose other numeric
/// fields may be, while JSON Schema requires numbers there. Only keywords at
/// schema positions are touched, so a property that happens to be called
/// `type`, `nullable` or `minItems` is left alone.
pub(crate) fn from_gemini_schema(schema: &Value) -> Value {
    let mut out = schema.clone();
    normalize_dialect(&mut out);
    out
}

/// `int64` constraints of Gemini's `Schema` message.
const INTEGER_CONSTRAINTS: [&str; 6] = [
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "minProperties",
    "maxProperties",
];
/// `double` constraints of Gemini's `Schema` message.
const NUMBER_CONSTRAINTS: [&str; 2] = ["minimum", "maximum"];

/// Turns a numeric constraint given as a decimal string into a JSON number.
/// Strings that are not numbers are left as they are.
fn destring_number(value: &mut Value, integer: bool) {
    let Value::String(text) = value else {
        return;
    };
    let text = text.trim();
    if let Ok(whole) = text.parse::<i64>() {
        *value = Value::from(whole);
    } else if !integer
        && let Some(number) = text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
    {
        *value = Value::Number(number);
    }
}

fn normalize_dialect(value: &mut Value) {
    let Value::Object(map) = value else {
        return;
    };
    for key in INTEGER_CONSTRAINTS {
        if let Some(constraint) = map.get_mut(key) {
            destring_number(constraint, true);
        }
    }
    for key in NUMBER_CONSTRAINTS {
        if let Some(constraint) = map.get_mut(key) {
            destring_number(constraint, false);
        }
    }
    match map.get_mut("type") {
        Some(Value::String(name)) => *name = name.to_ascii_lowercase(),
        Some(Value::Array(names)) => {
            for name in names.iter_mut() {
                if let Value::String(name) = name {
                    *name = name.to_ascii_lowercase();
                }
            }
        }
        _ => {}
    }
    map.shift_remove("propertyOrdering");
    if let Some(flag) = map.get("nullable").and_then(Value::as_bool) {
        map.shift_remove("nullable");
        if flag && let Some(Value::String(name)) = map.get("type").cloned() {
            map.insert(
                "type".to_string(),
                Value::Array(vec![Value::String(name), Value::String("null".to_string())]),
            );
        }
    }
    for (key, child) in map.iter_mut() {
        match key.as_str() {
            "properties" => {
                if let Value::Object(named) = child {
                    named
                        .iter_mut()
                        .for_each(|(_, schema)| normalize_dialect(schema));
                }
            }
            "items" | "additionalProperties" => normalize_dialect(child),
            "anyOf" | "oneOf" | "allOf" | "prefixItems" => {
                if let Value::Array(list) = child {
                    list.iter_mut().for_each(normalize_dialect);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod bounded_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn conditional_transform_consumes_the_source_branch() {
        let mut value = json!({"then": {"properties": {
            "outer": {"else": {"properties": {"inner": {"type": "string"}}}}
        }}});
        transform(&mut value, Mode::JsonSchema);
        assert!(value.get("then").is_none());
        let outer = &value["properties"]["outer"];
        assert!(outer.get("else").is_none());
        assert_eq!(outer["properties"]["inner"]["type"], "string");
    }

    #[test]
    fn exhausted_transform_allowance_produces_a_stable_stub() {
        let mut value = json!({"enum": [1, 2]});
        let mut budget = 0;
        transform_bounded(&mut value, Mode::Legacy, &mut budget);
        assert_eq!(value["type"], "object");
        assert_eq!(
            value["description"],
            "Schema omitted: transformation budget exceeded"
        );
        assert_eq!(sanitize_schema_legacy(&value), value);
        assert_eq!(budget, 0);
    }

    #[test]
    fn reference_aliases_share_measurement_and_recursion_identity() {
        let root = json!({"$defs": {"a~b": {"type": "string"}}});
        let mut ctx = RefCtx {
            root: &root,
            stack: Vec::new(),
            budget: 1024,
            measured: HashMap::new(),
        };
        assert!(ctx.take("#/$defs/a~0b", 0).is_some());
        assert!(ctx.take("#/$defs/a~b", 0).is_some());
        assert_eq!(ctx.measured.len(), 1);
        ctx.stack
            .push(std::ptr::from_ref(ctx.resolve("#/$defs/a~0b").unwrap()));
        assert!(ctx.take("#/$defs/a~b", 0).is_none());
    }
}
