//! `sanitize_schema` / `sanitize_schema_legacy`: every rule of the Gemini
//! schema cleaner.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_gemini::{sanitize_schema, sanitize_schema_legacy};

fn clean(schema: Value) -> Value {
    let once = sanitize_schema(&schema);
    // Cleaning must be idempotent: schemas can pass through it twice.
    assert_eq!(sanitize_schema(&once), once, "not idempotent for {schema}");
    once
}

fn clean_legacy(schema: Value) -> Value {
    let once = sanitize_schema_legacy(&schema);
    assert_eq!(
        sanitize_schema_legacy(&once),
        once,
        "legacy not idempotent for {schema}"
    );
    once
}

// ---------------------------------------------------------------------------
// Pass-through and metadata removal
// ---------------------------------------------------------------------------

#[test]
fn plain_schema_is_unchanged() {
    let schema = json!({
        "type": "object",
        "properties": {
            "city": {"type": "string", "description": "City name"},
            "days": {"type": "integer", "minimum": 1, "maximum": 14}
        },
        "required": ["city"]
    });
    assert_eq!(clean(schema.clone()), schema);
}

#[test]
fn json_schema_variant_keeps_constraints_and_additional_properties() {
    let schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "code": {"type": "string", "pattern": "^[A-Z]{3}$", "minLength": 3, "maxLength": 3},
            "amount": {"type": "number", "minimum": 0, "exclusiveMaximum": 100, "multipleOf": 0.5},
            "when": {"type": "string", "format": "date-time", "default": "now"},
            "tags": {"type": "array", "items": {"type": "string"}, "minItems": 1, "uniqueItems": true},
            "extra": {"type": "object", "additionalProperties": {"type": "string", "minLength": 1}}
        }
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "code": {"type": "string", "pattern": "^[A-Z]{3}$", "minLength": 3, "maxLength": 3},
                "amount": {"type": "number", "minimum": 0, "exclusiveMaximum": 100, "multipleOf": 0.5},
                "when": {"type": "string", "format": "date-time", "default": "now"},
                "tags": {"type": "array", "items": {"type": "string"}, "minItems": 1, "uniqueItems": true},
                "extra": {"type": "object", "additionalProperties": {"type": "string", "minLength": 1}}
            }
        })
    );
}

#[test]
fn metadata_keywords_are_removed_everywhere() {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://example.com/tool.json",
        "id": "legacy-id",
        "$anchor": "root",
        "$vocabulary": {"https://json-schema.org/draft/2020-12/vocab/core": true},
        "$dynamicAnchor": "node",
        "$comment": "internal",
        "title": "Tool",
        "deprecated": true,
        "prefill": "x",
        "encrypted": false,
        "type": "object",
        "properties": {
            "mode": {
                "type": "string",
                "title": "Mode",
                "$comment": "nested",
                "$dynamicRef": "#node",
                "enumDescriptions": ["fast", "slow"],
                "enumTitles": ["Fast", "Slow"],
                "encrypted": true
            }
        },
        "patternProperties": {"^x_": {"type": "string"}},
        "propertyNames": {"pattern": "^[a-z]+$"},
        "unevaluatedProperties": false,
        "contentSchema": {"type": "string"}
    });
    assert_eq!(
        clean(schema),
        json!({"type": "object", "properties": {"mode": {"type": "string"}}})
    );
}

#[test]
fn properties_named_like_keywords_survive() {
    let schema = json!({
        "type": "object",
        "properties": {
            "$id": {"type": "string"},
            "$comment": {"type": "string"},
            "$schema": {"type": "string"},
            "id": {"type": "integer"},
            "title": {"type": "string"},
            "encrypted": {"type": "boolean", "encrypted": true},
            "enumDescriptions": {"type": "string"},
            "x-data": {"type": "string"},
            "propertyNames": {"type": "string"},
            "patternProperties": {"type": "string"},
            "items": {"type": "string"},
            "pattern": {"type": "string"},
            "const": {"type": "string"},
            "$ref": {"type": "string"},
            "nullable": {"type": "boolean"}
        },
        "required": ["x-data", "id"]
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {
                "$id": {"type": "string"},
                "$comment": {"type": "string"},
                "$schema": {"type": "string"},
                "id": {"type": "integer"},
                "title": {"type": "string"},
                "encrypted": {"type": "boolean"},
                "enumDescriptions": {"type": "string"},
                "x-data": {"type": "string"},
                "propertyNames": {"type": "string"},
                "patternProperties": {"type": "string"},
                "items": {"type": "string"},
                "pattern": {"type": "string"},
                "const": {"type": "string"},
                "$ref": {"type": "string"},
                "nullable": {"type": "boolean"}
            },
            "required": ["x-data", "id"]
        })
    );
}

#[test]
fn a_property_called_properties_is_a_property() {
    let schema = json!({
        "type": "object",
        "properties": {
            "properties": {
                "type": "object",
                "title": "drop me",
                "properties": {"title": {"type": "string", "title": "drop me too"}},
                "propertyNames": {"pattern": "^t"}
            }
        }
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {
                "properties": {"type": "object", "properties": {"title": {"type": "string"}}}
            }
        })
    );
}

#[test]
fn extension_keys_are_removed_but_not_property_names() {
    let schema = json!({
        "type": "object",
        "x-root.meta": {"owner": "team"},
        "x-order": 3,
        "properties": {
            "foo.bar": {"type": "string", "x-internal": {"deep": {"x-deeper": 1}}},
            "x-data": {"type": "string"}
        },
        "required": ["x-data"]
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {"foo.bar": {"type": "string"}, "x-data": {"type": "string"}},
            "required": ["x-data"]
        })
    );
}

#[test]
fn required_only_names_existing_properties() {
    let schema = json!({
        "type": "object",
        "properties": {"country": {"type": "string"}, "industry": {"type": "string"}},
        "required": ["country", "industry", "stale_field", "another_stale"]
    });
    assert_eq!(clean(schema)["required"], json!(["country", "industry"]));

    let none_left =
        json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["b"]});
    assert_eq!(
        clean(none_left),
        json!({"type": "object", "properties": {"a": {"type": "string"}}})
    );

    let no_properties = json!({"type": "object", "required": ["a"]});
    assert_eq!(clean(no_properties), json!({"type": "object"}));
}

#[test]
fn data_values_are_not_treated_as_schema() {
    let schema = json!({
        "type": "object",
        "properties": {
            "config": {
                "type": "object",
                "default": {"title": "keep", "$ref": "#/nope", "x-flag": true, "const": 1},
                "examples": [{"title": "keep", "type": ["a", "b"]}]
            },
            "kind": {"enum": [{"title": "t"}]}
        }
    });
    let out = clean(schema);
    assert_eq!(
        out["properties"]["config"]["default"],
        json!({"title": "keep", "$ref": "#/nope", "x-flag": true, "const": 1})
    );
    assert_eq!(
        out["properties"]["config"]["examples"],
        json!([{"title": "keep", "type": ["a", "b"]}])
    );
}

#[test]
fn a_request_body_is_never_rewritten() {
    for body in [
        json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}], "title": "x"}),
        json!({"messages": [{"role": "user", "content": "hi"}], "const": 1}),
        json!({"tools": [{"functionDeclarations": []}], "title": "x"}),
        json!({"functionDeclarations": [{"name": "f"}]}),
        json!({"request": {"contents": []}, "title": "x"}),
    ] {
        assert_eq!(sanitize_schema(&body), body);
        assert_eq!(sanitize_schema_legacy(&body), body);
    }
}

#[test]
fn non_schema_values_are_returned_as_they_are() {
    assert_eq!(sanitize_schema(&json!(true)), json!({}));
    assert_eq!(sanitize_schema(&json!(false)), json!(false));
    assert_eq!(sanitize_schema(&json!(null)), json!(null));
    assert_eq!(sanitize_schema(&json!("x")), json!("x"));
    assert_eq!(sanitize_schema(&json!({})), json!({}));
}

#[test]
fn big_integers_keep_their_digits() {
    let schema = json!({"type": "integer", "maximum": 9_007_199_254_740_993u64});
    assert_eq!(
        clean(schema).to_string(),
        r#"{"type":"integer","maximum":9007199254740993}"#
    );
}

// ---------------------------------------------------------------------------
// Phase 0: repair
// ---------------------------------------------------------------------------

#[test]
fn schema_wrapper_is_unwrapped_and_rewrapped() {
    assert_eq!(clean(json!({"schema": true})), json!({"schema": {}}));
    assert_eq!(
        clean(json!({"schema": {"type": "object", "title": "T", "properties": {"a": true}}})),
        json!({"schema": {"type": "object", "properties": {"a": {}}}})
    );
}

#[test]
fn true_subschemas_become_empty_objects_and_false_stays() {
    let schema = json!({
        "type": "object",
        "properties": {
            "anything": true,
            "nothing": false,
            "list": {"type": "array", "items": true},
            "tuple": {"type": "array", "items": [true, {"type": "string"}]},
            "union": {"anyOf": [true, {"type": "string"}]}
        },
        "allOf": [true]
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {
                "anything": {},
                "nothing": false,
                "list": {"type": "array", "items": {}},
                "tuple": {"type": "array", "items": [{}, {"type": "string"}]},
                "union": {"type": "string"}
            }
        })
    );
}

#[test]
fn bare_property_maps_move_under_properties() {
    assert_eq!(
        clean(json!({"path": {"type": "string"}, "depth": {"type": "integer"}})),
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}, "depth": {"type": "integer"}}
        })
    );
}

#[test]
fn bare_property_maps_nested_and_with_keyword_looking_names() {
    let schema = json!({
        "description": "Top",
        "title": {"type": "string"},
        "description_text": {"type": "string"},
        "format": {"type": "string"},
        "type": {"type": "string"},
        "options": {"verbose": {"type": "boolean"}, "limits": {"max": {"type": "integer"}}}
    });
    assert_eq!(
        clean(schema),
        json!({
            "description": "Top",
            "type": "object",
            "properties": {
                "title": {"type": "string"},
                "description_text": {"type": "string"},
                "format": {"type": "string"},
                "type": {"type": "string"},
                "options": {
                    "type": "object",
                    "properties": {
                        "verbose": {"type": "boolean"},
                        "limits": {"type": "object", "properties": {"max": {"type": "integer"}}}
                    }
                }
            }
        })
    );
}

#[test]
fn bare_property_maps_with_request_looking_names_and_explicit_type() {
    let schema = json!({
        "type": "object",
        "request": {"type": "string"},
        "headers": {"type": "object"},
        "tools": {"type": "array", "items": {"type": "string"}}
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {
                "request": {"type": "string"},
                "headers": {"type": "object"},
                "tools": {"type": "array", "items": {"type": "string"}}
            }
        })
    );
}

#[test]
fn orphan_property_joins_existing_properties_without_overwriting() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}, "b": {"type": "integer"}},
        "b": {"type": "string"},
        "c": {"type": "boolean"}
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "integer"}, "c": {"type": "boolean"}}
        })
    );
}

#[test]
fn bare_property_maps_inside_array_items_and_with_nullable() {
    let schema = json!({
        "type": "array",
        "items": {"nullable": true, "name": {"type": "string"}}
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "array",
            "items": {"type": "object", "properties": {"name": {"type": "string"}}}
        })
    );
}

#[test]
fn object_valued_keywords_are_not_properties() {
    let schema = json!({
        "type": "object",
        "default": {"a": 1},
        "additionalProperties": {"type": "string"},
        "discriminator": {"propertyName": "kind"},
        "not": {"type": "null"},
        "x-meta": {"k": "v"}
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "default": {"a": 1},
            "additionalProperties": {"type": "string"},
            "discriminator": {"propertyName": "kind"},
            "not": {"type": "null"}
        })
    );
}

#[test]
fn typed_non_object_nodes_do_not_grow_properties() {
    let schema = json!({"type": "string", "enum": ["a"], "x-labels": {"a": "A"}, "meta": {"k": 1}});
    assert_eq!(
        clean(schema),
        json!({"type": "string", "enum": ["a"], "meta": {"k": 1}})
    );
}

#[test]
fn boolean_required_on_a_property_is_promoted() {
    let schema = json!({
        "type": "object",
        "properties": {
            "zeta": {"type": "string", "required": true},
            "alpha": {"type": "string", "required": true},
            "optional": {"type": "string", "required": false},
            "existing": {"type": "string"}
        },
        "required": ["existing"]
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {
                "zeta": {"type": "string"},
                "alpha": {"type": "string"},
                "optional": {"type": "string"},
                "existing": {"type": "string"}
            },
            "required": ["existing", "alpha", "zeta"]
        })
    );
}

#[test]
fn arrays_get_items_and_items_imply_array() {
    assert_eq!(
        clean(json!({"type": "array"})),
        json!({"type": "array", "items": {"type": "string"}})
    );
    assert_eq!(
        clean(json!({"items": {"type": "integer"}})),
        json!({"items": {"type": "integer"}, "type": "array"})
    );
    assert_eq!(
        clean(
            json!({"type": "object", "properties": {"list": {"type": ["array", "null"]}}, "required": ["list"]})
        ),
        json!({
            "type": "object",
            "properties": {"list": {"type": "array", "items": {"type": "string"}, "description": "(nullable)"}}
        })
    );
}

#[test]
fn items_only_stay_on_arrays() {
    assert_eq!(
        clean(json!({"type": "string", "items": {"type": "string"}})),
        json!({"type": "string"})
    );
    assert_eq!(
        clean(
            json!({"type": "object", "items": {"type": "string"}, "properties": {"items": {"type": "string"}}})
        ),
        json!({"type": "object", "properties": {"items": {"type": "string"}}})
    );
    assert_eq!(
        clean(json!({"type": ["string", "array"], "items": {"type": "string"}})),
        json!({"type": "array", "items": {"type": "string"}, "description": "Accepts: string | array"})
    );
    // Gemini-style upper-case types keep their items.
    assert_eq!(
        clean(json!({"type": "ARRAY", "items": {"type": "STRING"}})),
        json!({"type": "ARRAY", "items": {"type": "STRING"}})
    );
}

// ---------------------------------------------------------------------------
// Phase 1: $ref, const, enum
// ---------------------------------------------------------------------------

#[test]
fn local_refs_are_inlined() {
    let schema = json!({
        "type": "object",
        "properties": {
            "home": {"$ref": "#/$defs/Address"},
            "work": {"$ref": "#/definitions/Address", "description": "Work address"}
        },
        "$defs": {"Address": {"type": "object", "title": "Address", "properties": {"street": {"type": "string"}}}},
        "definitions": {"Address": {"type": "object", "properties": {"city": {"type": "string"}}, "description": "generic"}}
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {
                "home": {"type": "object", "properties": {"street": {"type": "string"}}},
                "work": {"type": "object", "properties": {"city": {"type": "string"}}, "description": "Work address"}
            }
        })
    );
}

#[test]
fn root_ref_and_chained_refs_are_followed() {
    let schema = json!({
        "$ref": "#/definitions/Query",
        "definitions": {
            "Query": {"type": "object", "properties": {"sort": {"$ref": "#/definitions/Sort"}}},
            "Sort": {"$ref": "#/definitions/Direction"},
            "Direction": {"type": "string", "enum": ["asc", "desc"]}
        }
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {"sort": {"type": "string", "enum": ["asc", "desc"], "description": "Allowed: asc, desc"}}
        })
    );
}

#[test]
fn recursive_dangling_and_remote_refs_become_stubs() {
    let schema = json!({
        "type": "object",
        "properties": {
            "tree": {"$ref": "#/$defs/Node"},
            "missing": {"$ref": "#/$defs/Nope", "description": "A thing"},
            "remote": {"$ref": "https://example.com/schemas/Pet.json"},
            "escaped": {"$ref": "#/$defs/a~1b"}
        },
        "$defs": {
            "Node": {
                "type": "object",
                "properties": {"children": {"type": "array", "items": {"$ref": "#/$defs/Node"}}}
            }
        }
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {
                "tree": {
                    "type": "object",
                    "properties": {
                        "children": {"type": "array", "items": {"type": "object", "description": "See: Node"}}
                    }
                },
                "missing": {"type": "object", "description": "A thing (See: Nope)"},
                "remote": {"type": "object", "description": "See: Pet.json"},
                "escaped": {"type": "object", "description": "See: a/b"}
            }
        })
    );
}

#[test]
fn const_becomes_a_single_value_enum() {
    assert_eq!(
        clean(json!({"type": "object", "properties": {"kind": {"const": "circle"}}})),
        json!({"type": "object", "properties": {"kind": {"enum": ["circle"], "type": "string"}}})
    );
    // An existing enum wins over const.
    assert_eq!(
        clean(json!({"const": "a", "enum": ["a", "b"]})),
        json!({"enum": ["a", "b"], "type": "string", "description": "Allowed: a, b"})
    );
}

#[test]
fn enums_are_stringified_and_typed_as_string() {
    assert_eq!(
        clean(json!({"type": "integer", "enum": [1, 2, 3], "description": "Level"})),
        json!({"type": "string", "enum": ["1", "2", "3"], "description": "Level (Allowed: 1, 2, 3)"})
    );
    assert_eq!(
        clean(json!({"enum": [true, null, 1.5, "x"]})),
        json!({"enum": ["true", "", "1.5", "x"], "type": "string", "description": "Allowed: true, , 1.5, x"})
    );
}

#[test]
fn enum_hint_only_for_two_to_ten_values() {
    assert_eq!(
        clean(json!({"enum": ["only"]})),
        json!({"enum": ["only"], "type": "string"})
    );
    let eleven: Vec<String> = (0..11).map(|i| format!("v{i}")).collect();
    let out = clean(json!({"enum": eleven}));
    assert!(out.get("description").is_none());
    let ten: Vec<String> = (0..10).map(|i| format!("v{i}")).collect();
    let out = clean(json!({"enum": ten}));
    assert_eq!(
        out["description"],
        json!("Allowed: v0, v1, v2, v3, v4, v5, v6, v7, v8, v9")
    );
}

#[test]
fn nullable_enum_property_is_optional() {
    let schema = json!({
        "type": "object",
        "properties": {"unit": {"type": ["string", "null"], "enum": ["c", "f", null]}},
        "required": ["unit"]
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {
                "unit": {"type": "string", "enum": ["c", "f", ""], "description": "Allowed: c, f,  (nullable)"}
            }
        })
    );
}

// ---------------------------------------------------------------------------
// Phase 2: flatten
// ---------------------------------------------------------------------------

#[test]
fn conditionals_contribute_their_properties() {
    let schema = json!({
        "type": "object",
        "properties": {"kind": {"type": "string"}},
        "if": {"properties": {"kind": {"const": "file"}}},
        "then": {"properties": {"path": {"type": "string", "description": "File path"}}, "required": ["path"]},
        "else": {"properties": {"url": {"type": "string"}, "kind": {"type": "integer"}}}
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string"},
                "path": {"type": "string", "description": "File path"},
                "url": {"type": "string"}
            }
        })
    );
}

#[test]
fn conditionals_inside_all_of_and_nested_properties() {
    let schema = json!({
        "type": "object",
        "properties": {
            "inner": {
                "type": "object",
                "properties": {"a": {"type": "string"}},
                "if": {"required": ["a"]},
                "then": {"properties": {"b": {"type": "integer", "description": "B"}}}
            }
        },
        "allOf": [{"if": {"required": ["inner"]}, "then": {"properties": {"extra": {"type": "boolean"}}}}]
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {
                "inner": {
                    "type": "object",
                    "properties": {"a": {"type": "string"}, "b": {"type": "integer", "description": "B"}}
                },
                "extra": {"type": "boolean"}
            }
        })
    );
}

#[test]
fn all_of_is_merged_without_overwriting() {
    let schema = json!({
        "description": "Parent",
        "allOf": [
            {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"], "description": "ignored"},
            {"properties": {"a": {"type": "integer", "description": "fills a gap"}, "b": {"type": "boolean"}}, "required": ["b", "a"]}
        ]
    });
    assert_eq!(
        clean(schema),
        json!({
            "description": "Parent",
            "required": ["a", "b"],
            "type": "object",
            "properties": {"a": {"type": "string", "description": "fills a gap"}, "b": {"type": "boolean"}}
        })
    );
}

#[test]
fn any_of_picks_the_most_structured_branch() {
    assert_eq!(
        clean(json!({
            "description": "Parent desc",
            "anyOf": [{"type": "string", "description": "Child desc"}, {"type": "integer"}]
        })),
        json!({"type": "string", "description": "Parent desc (Child desc) (Accepts: string | integer)"})
    );
    assert_eq!(
        clean(json!({"anyOf": [
            {"type": "string"},
            {"type": "array", "items": {"type": "string"}},
            {"properties": {"q": {"type": "string"}}}
        ]})),
        json!({"properties": {"q": {"type": "string"}}, "description": "Accepts: string | array | object"})
    );
}

#[test]
fn one_of_behaves_like_any_of_and_parent_keywords_survive() {
    assert_eq!(
        clean(json!({
            "default": "x",
            "description": "Same",
            "oneOf": [{"type": "number", "description": "Same"}, {"type": "number", "minimum": 0}]
        })),
        json!({"type": "number", "description": "Same", "default": "x"})
    );
}

#[test]
fn union_with_null_makes_the_property_optional() {
    let schema = json!({
        "type": "object",
        "properties": {
            "limit": {"anyOf": [{"type": "integer"}, {"type": "null"}], "description": "Max rows"},
            "name": {"type": "string"}
        },
        "required": ["limit", "name"]
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "description": "Max rows (nullable)"},
                "name": {"type": "string"}
            },
            "required": ["name"]
        })
    );
}

#[test]
fn union_on_an_object_with_properties_merges_branch_properties() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}},
        "anyOf": [
            {"properties": {"b": {"type": "integer"}}, "required": ["b"]},
            {"properties": {"a": {"type": "boolean"}, "c": {"type": "number"}}},
            {"type": "null"}
        ]
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "integer"}, "c": {"type": "number"}}
        })
    );
}

#[test]
fn union_of_required_groups_collapses_cleanly() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
        "anyOf": [{"required": ["a"]}, {"required": ["b"]}]
    });
    assert_eq!(
        clean(schema),
        json!({"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "string"}}})
    );
}

#[test]
fn type_arrays_collapse_to_one_type() {
    let schema = json!({
        "type": "object",
        "properties": {
            "maybe": {"type": ["string", "null"], "description": "Optional text"},
            "either": {"type": ["integer", "string"]},
            "only_null": {"type": ["null"]},
            "plain": {"type": "string"}
        },
        "required": ["maybe", "either", "plain"]
    });
    assert_eq!(
        clean(schema),
        json!({
            "type": "object",
            "properties": {
                "maybe": {"type": "string", "description": "Optional text (nullable)"},
                "either": {"type": "integer", "description": "Accepts: integer | string"},
                "only_null": {"type": "string", "description": "(nullable)"},
                "plain": {"type": "string"}
            },
            "required": ["either", "plain"]
        })
    );
}

#[test]
fn nullable_keyword_on_a_property_makes_it_optional() {
    let schema = json!({
        "type": "object",
        "properties": {"note": {"type": "string", "nullable": true}},
        "required": ["note"]
    });
    assert_eq!(
        clean(schema),
        json!({"type": "object", "properties": {"note": {"type": "string", "description": "(nullable)"}}})
    );
}

// ---------------------------------------------------------------------------
// Phase 3: placeholders
// ---------------------------------------------------------------------------

#[test]
fn placeholder_properties_are_removed() {
    assert_eq!(
        clean(
            json!({"type": "object", "properties": {"_": {"type": "string"}}, "required": ["_"]})
        ),
        json!({"type": "object", "properties": {}})
    );
    assert_eq!(
        clean(json!({
            "type": "object",
            "properties": {"reason": {"type": "string", "description": "Brief explanation of why you are calling this tool"}},
            "required": ["reason"]
        })),
        json!({"type": "object", "properties": {}})
    );
    // A real `reason` parameter stays.
    let real = json!({
        "type": "object",
        "properties": {"reason": {"type": "string", "description": "Why the refund is requested"}},
        "required": ["reason"]
    });
    assert_eq!(clean(real.clone()), real);
    let with_sibling = json!({
        "type": "object",
        "properties": {
            "reason": {"type": "string", "description": "Brief explanation of why you are calling this tool"},
            "id": {"type": "string"}
        }
    });
    assert_eq!(clean(with_sibling.clone()), with_sibling);
}

// ---------------------------------------------------------------------------
// Legacy (`parameters`) variant
// ---------------------------------------------------------------------------

#[test]
fn legacy_turns_unsupported_constraints_into_hints() {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "code": {"type": "string", "pattern": "^[A-Z]{3}$", "minLength": 3, "maxLength": 3, "description": "Airport"},
            "price": {"type": "number", "minimum": 0, "exclusiveMaximum": 100, "multipleOf": 0.5},
            "tags": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 5, "uniqueItems": true},
            "mode": {"type": "string", "default": "fast", "examples": ["fast", "slow"]}
        },
        "required": ["code"]
    });
    assert_eq!(
        clean_legacy(schema),
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Airport (minLength: 3) (maxLength: 3) (pattern: ^[A-Z]{3}$)"
                },
                "price": {
                    "type": "number",
                    "minimum": 0,
                    "description": "exclusiveMaximum: 100 (multipleOf: 0.5)"
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "minItems: 1 (maxItems: 5) (uniqueItems: true)"
                },
                "mode": {"type": "string", "description": "default: fast (examples: [\"fast\",\"slow\"])"}
            },
            "required": ["code"],
            "description": "No extra properties allowed"
        })
    );
}

#[test]
fn legacy_keeps_only_formats_the_dialect_knows() {
    let schema = json!({
        "type": "object",
        "properties": {
            "at": {"type": "string", "format": "date-time"},
            "site": {"type": "string", "format": "uri"},
            "count": {"type": "integer", "format": "int64"},
            "ratio": {"type": "number", "format": "double"},
            "odd": {"type": "integer", "format": "uint8"}
        }
    });
    assert_eq!(
        clean_legacy(schema),
        json!({
            "type": "object",
            "properties": {
                "at": {"type": "string", "format": "date-time"},
                "site": {"type": "string", "description": "format: uri"},
                "count": {"type": "integer", "format": "int64"},
                "ratio": {"type": "number", "format": "double"},
                "odd": {"type": "integer", "description": "format: uint8"}
            }
        })
    );
}

#[test]
fn legacy_removes_keywords_outside_the_schema_message() {
    let schema = json!({
        "type": "object",
        "properties": {
            "a": {"type": "string", "not": {"const": "x"}, "readOnly": true},
            "b": {"type": "array", "prefixItems": [{"type": "string"}], "items": {"type": "string"}, "contains": {"type": "string"}}
        },
        "additionalProperties": {"type": "string"},
        "dependentRequired": {"a": ["b"]},
        "minProperties": 1
    });
    assert_eq!(
        clean_legacy(schema),
        json!({
            "type": "object",
            "properties": {
                "a": {"type": "string"},
                "b": {"type": "array", "items": {"type": "string"}, "description": "contains: {\"type\":\"string\"}"}
            },
            "minProperties": 1
        })
    );
}

#[test]
fn legacy_applies_the_shared_rules_too() {
    let schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "T",
        "type": "object",
        "properties": {
            "kind": {"const": "a"},
            "maybe": {"type": ["string", "null"]},
            "ref": {"$ref": "#/$defs/R"}
        },
        "required": ["kind", "maybe", "gone"],
        "$defs": {"R": {"type": "integer", "title": "R"}}
    });
    assert_eq!(
        clean_legacy(schema),
        json!({
            "type": "object",
            "properties": {
                "kind": {"enum": ["a"], "type": "string"},
                "maybe": {"type": "string", "description": "(nullable)"},
                "ref": {"type": "integer"}
            },
            "required": ["kind"]
        })
    );
}
