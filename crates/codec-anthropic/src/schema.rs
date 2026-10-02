//! Fitting a JSON Schema written for another protocol to what Anthropic's
//! structured outputs (`output_config.format`) accept.
//!
//! The Messages API compiles the schema into a grammar and takes a narrow
//! dialect only; anything outside it is answered with a 400, which is a
//! request fault and so cannot be repaired by failing over:
//!
//! * every object schema must say `additionalProperties: false`;
//! * numeric constraints (`minimum`, `maximum`, `multipleOf`, …), string
//!   length constraints and array constraints other than `minItems` 0 or 1
//!   are not supported;
//! * `format` is limited to a handful of values and `pattern` to regular
//!   expressions without look-around and back-references;
//! * enums hold scalars only, references stay inside the document and may
//!   not be recursive;
//! * a request may hold at most 24 optional properties and 16 union-typed
//!   ones.
//!
//! Schemas of Gemini clients (`responseSchema` never says
//! `additionalProperties`, and routinely carries `minimum` / `maxLength` /
//! `minItems`), of OpenAI clients without `strict`, and of Responses clients
//! are all outside that dialect. [`fit_output_schema`] rewrites such a schema
//! the way Anthropic's own SDKs do: unsupported constraints are removed and
//! recorded in the `description` so the model still reads them, and objects
//! are closed. A schema that cannot be made acceptable yields `None`; the
//! caller then describes it in a system instruction instead.

use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

/// Nesting beyond which a schema is not fitted (it is described instead).
const MAX_DEPTH: usize = 48;

/// Most optional properties a request's schemas may hold.
const MAX_OPTIONAL_PROPERTIES: usize = 24;

/// Most union-typed (`anyOf`, type array) schemas a request may hold.
const MAX_UNION_TYPES: usize = 16;

/// `format` values the grammar understands.
const SUPPORTED_FORMATS: &[&str] = &[
    "date-time",
    "time",
    "date",
    "duration",
    "email",
    "hostname",
    "uri",
    "ipv4",
    "ipv6",
    "uuid",
];

/// Constraints the grammar cannot enforce. They move into the description.
const UNSUPPORTED_CONSTRAINTS: &[&str] = &[
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
];

/// Whether a schema's `type` is, or includes, `name`.
fn has_type(schema: &Map<String, Value>, name: &str) -> bool {
    match schema.get("type") {
        Some(Value::String(kind)) => kind == name,
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some(name)),
        _ => false,
    }
}

fn is_scalar(value: &Value) -> bool {
    !matches!(value, Value::Object(_) | Value::Array(_))
}

/// Whether the grammar can compile `pattern`: no look-around, no
/// back-references, no word boundaries.
fn is_supported_pattern(pattern: &str) -> bool {
    if ["(?=", "(?!", "(?<", "\\b", "\\B"]
        .iter()
        .any(|construct| pattern.contains(construct))
    {
        return false;
    }
    let bytes = pattern.as_bytes();
    !bytes
        .windows(2)
        .any(|pair| pair[0] == b'\\' && (b'1'..=b'9').contains(&pair[1]))
}

/// The name of the definition a local reference points at; `Some("")` for a
/// reference to the document itself. `None` for anything else.
fn local_definition(reference: &str) -> Option<&str> {
    if reference == "#" {
        return Some("");
    }
    reference
        .strip_prefix("#/$defs/")
        .or_else(|| reference.strip_prefix("#/definitions/"))
        .filter(|name| !name.is_empty() && !name.contains('/'))
}

/// Collects the definitions `schema` refers to, at any depth.
fn references<'a>(schema: &'a Value, depth: usize, out: &mut Vec<&'a str>) {
    if depth > MAX_DEPTH * 2 {
        return;
    }
    match schema {
        Value::Object(map) => {
            for (key, value) in map {
                match value {
                    Value::String(reference) if key == "$ref" => out.push(reference),
                    other => references(other, depth + 1, out),
                }
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| references(item, depth + 1, out)),
        _ => {}
    }
}

/// Whether every reference of `root` stays inside the document, names a
/// definition that exists and takes part in no cycle.
fn references_are_usable(root: &Map<String, Value>) -> bool {
    let mut definitions: HashMap<&str, &Value> = HashMap::new();
    for holder in ["$defs", "definitions"] {
        if let Some(Value::Object(entries)) = root.get(holder) {
            for (name, schema) in entries {
                definitions.entry(name.as_str()).or_insert(schema);
            }
        }
    }
    let mut found = Vec::new();
    for (key, value) in root {
        match value {
            Value::String(reference) if key == "$ref" => found.push(reference.as_str()),
            other => references(other, 0, &mut found),
        }
    }
    if targets(&found, &definitions).is_none() {
        return false;
    }
    let mut graph: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, schema) in &definitions {
        let mut found = Vec::new();
        references(schema, 0, &mut found);
        match targets(&found, &definitions) {
            Some(targets) => {
                graph.insert(name, targets);
            }
            None => return false,
        }
    }
    // Depth-first search for a cycle among the definitions.
    let mut done: HashSet<&str> = HashSet::new();
    for start in graph.keys().copied() {
        if done.contains(start) {
            continue;
        }
        let mut path: Vec<(&str, usize)> = vec![(start, 0)];
        let mut on_path: HashSet<&str> = HashSet::from([start]);
        while let Some((node, next)) = path.last().copied() {
            let target = graph
                .get(node)
                .and_then(|targets| targets.get(next))
                .copied();
            match target {
                Some(target) => {
                    if let Some(last) = path.last_mut() {
                        last.1 += 1;
                    }
                    if on_path.contains(target) {
                        return false;
                    }
                    if !done.contains(target) {
                        on_path.insert(target);
                        path.push((target, 0));
                    }
                }
                None => {
                    on_path.remove(node);
                    done.insert(node);
                    path.pop();
                }
            }
        }
    }
    true
}

/// The definitions a list of references points at. `None` when one of them
/// leaves the document, names a definition that does not exist, or refers to
/// the whole document (which is recursion).
fn targets<'a>(
    references: &[&'a str],
    definitions: &HashMap<&'a str, &'a Value>,
) -> Option<Vec<&'a str>> {
    references
        .iter()
        .map(|reference| match local_definition(reference) {
            Some("") | None => None,
            Some(name) => definitions.contains_key(name).then_some(name),
        })
        .collect()
}

#[derive(Default)]
struct Fitter {
    optional_properties: usize,
    union_types: usize,
}

impl Fitter {
    fn fit_list(&mut self, value: &Value, depth: usize) -> Option<Value> {
        let Value::Array(items) = value else {
            return None;
        };
        if items.is_empty() {
            return None;
        }
        items
            .iter()
            .map(|item| self.fit(item, depth + 1))
            .collect::<Option<Vec<Value>>>()
            .map(Value::Array)
    }

    fn fit_map(&mut self, value: &Value, depth: usize) -> Option<Value> {
        let Value::Object(entries) = value else {
            return None;
        };
        let mut out = Map::new();
        for (name, schema) in entries {
            out.insert(name.clone(), self.fit(schema, depth + 1)?);
        }
        Some(Value::Object(out))
    }

    fn fit(&mut self, schema: &Value, depth: usize) -> Option<Value> {
        // A boolean schema ("anything" / "nothing") has no grammar.
        let Value::Object(map) = schema else {
            return None;
        };
        if depth > MAX_DEPTH {
            return None;
        }
        let is_object = has_type(map, "object") || map.contains_key("properties");
        let mut out = Map::new();
        // Constraints that are removed, kept for the description.
        let mut notes = Map::new();
        for (key, value) in map {
            match key.as_str() {
                "type" => match value {
                    Value::String(_) => {
                        out.insert(key.clone(), value.clone());
                    }
                    Value::Array(kinds) if kinds.iter().all(Value::is_string) => {
                        if kinds.len() > 1 {
                            self.union_types += 1;
                        }
                        out.insert(key.clone(), value.clone());
                    }
                    _ => return None,
                },
                "properties" => {
                    out.insert(key.clone(), self.fit_map(value, depth)?);
                }
                "required" => {
                    if let Value::Array(names) = value
                        && names.iter().all(Value::is_string)
                    {
                        out.insert(key.clone(), value.clone());
                    }
                }
                // The tuple form of `items` is not supported.
                "items" => {
                    out.insert(key.clone(), self.fit(value, depth + 1)?);
                }
                "anyOf" | "oneOf" => {
                    self.union_types += 1;
                    out.insert("anyOf".to_string(), self.fit_list(value, depth)?);
                }
                "allOf" => {
                    out.insert(key.clone(), self.fit_list(value, depth)?);
                }
                "$defs" | "definitions" => {
                    out.insert(key.clone(), self.fit_map(value, depth)?);
                }
                "$ref" => {
                    out.insert(key.clone(), value.clone());
                }
                "enum" => {
                    let Value::Array(values) = value else {
                        return None;
                    };
                    if values.is_empty() || !values.iter().all(is_scalar) {
                        return None;
                    }
                    out.insert(key.clone(), value.clone());
                }
                "const" => {
                    if !is_scalar(value) {
                        return None;
                    }
                    out.insert(key.clone(), value.clone());
                }
                "description" | "title" | "default" => {
                    out.insert(key.clone(), value.clone());
                }
                "format" => match value.as_str() {
                    Some(format) if SUPPORTED_FORMATS.contains(&format) => {
                        out.insert(key.clone(), value.clone());
                    }
                    _ => {
                        notes.insert(key.clone(), value.clone());
                    }
                },
                "pattern" => match value.as_str() {
                    Some(pattern) if is_supported_pattern(pattern) => {
                        out.insert(key.clone(), value.clone());
                    }
                    _ => {
                        notes.insert(key.clone(), value.clone());
                    }
                },
                "minItems" => match value.as_u64() {
                    Some(0 | 1) => {
                        out.insert(key.clone(), value.clone());
                    }
                    _ => {
                        notes.insert(key.clone(), value.clone());
                    }
                },
                // Decided below, once the whole schema has been seen.
                "additionalProperties" | "nullable" => {}
                other if UNSUPPORTED_CONSTRAINTS.contains(&other) => {
                    notes.insert(key.clone(), value.clone());
                }
                // Annotations and keywords the grammar has no use for
                // (`$schema`, `examples`, `propertyOrdering`, …).
                _ => {}
            }
        }

        // OpenAPI's (and Gemini's) spelling of "or null".
        if map.get("nullable") == Some(&Value::Bool(true))
            && let Some(Value::String(kind)) = out.get("type").cloned()
            && kind != "null"
        {
            self.union_types += 1;
            out.insert("type".to_string(), json!([kind, "null"]));
        }
        if is_object {
            if !out.contains_key("type") {
                out.insert("type".to_string(), json!("object"));
            }
            let properties = out
                .get("properties")
                .and_then(Value::as_object)
                .map(|properties| properties.keys().collect::<Vec<_>>())
                .unwrap_or_default();
            let required: Vec<&str> = out
                .get("required")
                .and_then(Value::as_array)
                .map(|names| names.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            self.optional_properties += properties
                .iter()
                .filter(|name| !required.contains(&name.as_str()))
                .count();
            // A required name without a property cannot be generated.
            if required
                .iter()
                .any(|name| !properties.iter().any(|property| property == name))
            {
                let known: Vec<Value> = required
                    .iter()
                    .filter(|name| properties.iter().any(|property| property == *name))
                    .map(|name| Value::String((*name).to_string()))
                    .collect();
                out.insert("required".to_string(), Value::Array(known));
            }
            out.insert("additionalProperties".to_string(), Value::Bool(false));
        }
        if !notes.is_empty() {
            let note = Value::Object(notes).to_string();
            let description = match out.get("description").and_then(Value::as_str) {
                Some(existing) if !existing.trim().is_empty() => format!("{existing}\n\n{note}"),
                _ => note,
            };
            out.insert("description".to_string(), Value::String(description));
        }
        Some(Value::Object(out))
    }
}

/// Rewrites `schema` into the dialect `output_config.format` accepts (see the
/// module documentation). `None` when it cannot be done: the schema is not
/// an object schema document, refers outside itself or to itself, holds an
/// enum of objects, uses tuple arrays or boolean sub-schemas, nests too
/// deeply, or exceeds the API's limits on optional and union-typed
/// properties.
pub(crate) fn fit_output_schema(schema: &Value) -> Option<Value> {
    let root = schema.as_object()?;
    if !references_are_usable(root) {
        return None;
    }
    let mut fitter = Fitter::default();
    let fitted = fitter.fit(schema, 0)?;
    (fitter.optional_properties <= MAX_OPTIONAL_PROPERTIES && fitter.union_types <= MAX_UNION_TYPES)
        .then_some(fitted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn objects_are_closed_and_unsupported_constraints_move_to_the_description() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "maxLength": 40},
                "age": {"type": "integer", "minimum": 0, "maximum": 150, "description": "Years."},
                "tags": {"type": "array", "minItems": 2, "items": {
                    "type": "object", "properties": {"label": {"type": "string"}}
                }},
                "mail": {"type": "string", "format": "email"},
                "code": {"type": "string", "format": "byte", "pattern": "^(?=a)\\w+$"},
                "kind": {"enum": ["a", "b"]},
                "note": {"type": "string", "nullable": true}
            },
            "required": ["name", "ghost"],
            "propertyOrdering": ["name", "age"],
            "additionalProperties": {"type": "string"}
        });
        assert_eq!(
            fit_output_schema(&schema).unwrap(),
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "{\"maxLength\":40}"},
                    "age": {"type": "integer",
                            "description": "Years.\n\n{\"minimum\":0,\"maximum\":150}"},
                    "tags": {"type": "array", "description": "{\"minItems\":2}", "items": {
                        "type": "object", "properties": {"label": {"type": "string"}},
                        "additionalProperties": false
                    }},
                    "mail": {"type": "string", "format": "email"},
                    "code": {"type": "string",
                             "description": "{\"format\":\"byte\",\"pattern\":\"^(?=a)\\\\w+$\"}"},
                    "kind": {"enum": ["a", "b"]},
                    "note": {"type": ["string", "null"]}
                },
                "required": ["name"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn a_schema_in_the_dialect_is_left_as_it_is() {
        let schema = json!({
            "type": "object",
            "properties": {
                "answer": {"type": "string", "description": "The answer."},
                "parts": {"type": "array", "minItems": 1, "items": {"$ref": "#/$defs/part"}},
                "either": {"anyOf": [{"type": "string"}, {"type": "null"}]}
            },
            "required": ["answer", "parts", "either"],
            "additionalProperties": false,
            "$defs": {"part": {"type": "object", "properties": {"n": {"type": "integer"}},
                               "required": ["n"], "additionalProperties": false}}
        });
        assert_eq!(fit_output_schema(&schema).unwrap(), schema);
    }

    #[test]
    fn one_of_is_any_of_and_a_typeless_object_gets_its_type() {
        assert_eq!(
            fit_output_schema(&json!({
                "properties": {"v": {"oneOf": [{"type": "string"}, {"type": "number"}]}}
            }))
            .unwrap(),
            json!({
                "type": "object",
                "properties": {"v": {"anyOf": [{"type": "string"}, {"type": "number"}]}},
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn schemas_that_cannot_be_fitted_are_refused() {
        for schema in [
            json!("string"),
            json!(true),
            // Recursion, direct and through a definition.
            json!({"type": "object", "properties": {"child": {"$ref": "#"}}}),
            json!({"type": "object", "properties": {"n": {"$ref": "#/$defs/node"}},
                   "$defs": {"node": {"type": "object",
                                      "properties": {"next": {"$ref": "#/$defs/node"}}}}}),
            json!({"type": "object", "properties": {"n": {"$ref": "#/$defs/a"}},
                   "$defs": {"a": {"$ref": "#/$defs/b"}, "b": {"$ref": "#/$defs/a"}}}),
            // References that leave the document or point nowhere.
            json!({"type": "object", "properties": {"n": {"$ref": "https://example.com/s.json"}}}),
            json!({"type": "object", "properties": {"n": {"$ref": "#/$defs/missing"}}}),
            // Enums of objects, tuple arrays, boolean sub-schemas.
            json!({"type": "object", "properties": {"e": {"enum": [{"a": 1}]}}}),
            json!({"type": "array", "items": [{"type": "string"}, {"type": "number"}]}),
            json!({"type": "object", "properties": {"any": true}}),
        ] {
            assert_eq!(fit_output_schema(&schema), None, "{schema}");
        }
    }

    #[test]
    fn the_request_limits_are_respected() {
        let optional = |count: usize| {
            let properties: Map<String, Value> = (0..count)
                .map(|i| (format!("p{i}"), json!({"type": "string"})))
                .collect();
            json!({"type": "object", "properties": properties})
        };
        assert!(fit_output_schema(&optional(24)).is_some());
        assert_eq!(fit_output_schema(&optional(25)), None);

        let unions = |count: usize| {
            let properties: Map<String, Value> = (0..count)
                .map(|i| (format!("p{i}"), json!({"type": ["string", "null"]})))
                .collect();
            let required: Vec<String> = properties.keys().cloned().collect();
            json!({"type": "object", "properties": properties, "required": required})
        };
        assert!(fit_output_schema(&unions(16)).is_some());
        assert_eq!(fit_output_schema(&unions(17)), None);
    }

    #[test]
    fn deep_nesting_is_refused_without_overflowing_the_stack() {
        let mut schema = json!({"type": "string"});
        for _ in 0..200 {
            schema = json!({"type": "array", "items": schema});
        }
        assert_eq!(fit_output_schema(&schema), None);
    }
}
