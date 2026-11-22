// compile() and Validator: checks, errors, values, options and schemas from other sources.

use pi_rs_schema::{
    CompileError, CompileOptions, Draft, JsonSchema, UnresolvedRefs, ValidationError, Value,
    compile, compile_with,
};
use serde_json::json;

fn value(json: &serde_json::Value) -> Value {
    Value::from(json)
}

/// An object of the given members.
fn object(members: Vec<(&str, Value)>) -> Value {
    Value::Object(
        members
            .into_iter()
            .map(|(key, item)| (key.to_string(), item))
            .collect(),
    )
}

fn error(path: &str, keyword: &str, message: &str) -> ValidationError {
    ValidationError::new(path, keyword, message)
}

fn read() -> JsonSchema {
    serde_json::json!({
    "type": "object",
    "properties": { "path": { "type": "string" }, "offset": { "type": "integer", "minimum": 1 } },
    "required": ["path"],
    "additionalProperties": false
    })
}

fn block() -> JsonSchema {
    serde_json::json!({
    "anyOf": [
        {
            "type": "object",
            "properties": { "kind": { "const": "text" }, "text": { "type": "string" } },
            "required": ["kind", "text"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "properties": { "kind": { "const": "image" }, "width": { "type": "integer", "minimum": 0 } },
            "required": ["kind", "width"],
            "additionalProperties": false
        }
    ]
    })
}

#[test]
fn reports_a_missing_field_and_an_unknown_field_at_the_field() {
    let validator = compile(&read()).expect("the schema compiles");
    assert_eq!(
        validator.errors(&value(&json!({ "offset": 0, "extra": true }))),
        vec![
            error("/path", "required", "missing"),
            error("/offset", "minimum", "expected >= 1"),
            error("/extra", "additionalProperties", "unknown field"),
        ]
    );
}

#[test]
fn is_empty_for_a_valid_value() {
    let validator = compile(&read()).expect("the schema compiles");
    assert_eq!(
        validator.errors(&value(&json!({ "path": "a.ts" }))),
        Vec::new()
    );
}

#[test]
fn reports_the_branch_a_tagged_union_selects() {
    let validator =
        compile(&json!({ "type": "array", "items": block() })).expect("the schema compiles");
    assert_eq!(
        validator.errors(&value(
            &json!([{ "kind": "text", "text": "a" }, { "kind": "image", "width": -1 }])
        )),
        vec![error("/1/width", "minimum", "expected >= 0")]
    );
    assert_eq!(
        validator.errors(&value(&json!([{ "kind": "video" }]))),
        vec![error(
            "/0/kind",
            "anyOf",
            "expected one of [\"text\",\"image\"]"
        )]
    );
}

#[test]
fn escapes_a_slash_and_a_tilde_in_a_path_as_a_json_pointer_does() {
    let validator =
        compile(&json!({ "type": "object", "properties": { "a/b~c": { "type": "string" } } }))
            .expect("the schema compiles");
    assert_eq!(
        validator.errors(&value(&json!({ "a/b~c": 1 }))),
        vec![error("/a~1b~0c", "type", "expected string, got integer")]
    );
}

#[test]
fn follows_a_reference_into_the_referenced_schema() {
    let validator = compile(&json!({
        "$defs": { "point": { "type": "object", "properties": { "x": { "type": "number" } }, "required": ["x"] } },
        "type": "array",
        "items": { "$ref": "#/$defs/point" }
    }))
    .expect("the schema compiles");
    assert_eq!(
        validator.errors(&value(&json!([{ "x": 1 }, {}]))),
        vec![error("/1/x", "required", "missing")]
    );
}

#[test]
fn treats_a_property_set_to_undefined_as_absent() {
    let validator = compile(&read()).expect("the schema compiles");
    assert!(validator.check(&object(vec![
        ("path", Value::String("a.ts".to_string())),
        ("offset", Value::Undefined)
    ])));
    assert!(!validator.check(&object(vec![("path", Value::Undefined)])));
}

#[test]
fn checks_bytes_with_length_bounds() {
    let validator = compile(
        &json!({ "type": "object", "properties": { "data": { "type": "bytes", "maxLength": 4 } } }),
    )
    .expect("the schema compiles");
    assert!(validator.check(&object(vec![("data", Value::Bytes(vec![0; 4]))])));
    assert!(!validator.check(&object(vec![("data", Value::Bytes(vec![0; 5]))])));
    assert!(!validator.check(&value(&json!({ "data": [1, 2] }))));
}

#[test]
fn does_not_treat_bytes_as_an_object() {
    let validator = compile(&json!({ "type": "object" })).expect("the schema compiles");
    assert!(!validator.check(&Value::Bytes(vec![0])));
}

#[test]
fn checks_properties_named_like_object_prototype_members_as_own_properties() {
    let validator = compile(&json!({
        "type": "object",
        "properties": { "constructor": { "type": "string" }, "toString": { "type": "number" } },
        "required": ["constructor"],
        "additionalProperties": false
    }))
    .expect("the schema compiles");
    assert!(!validator.check(&value(&json!({}))));
    assert!(validator.check(&value(&json!({ "constructor": "a" }))));
    assert!(!validator.check(&value(&json!({ "constructor": "a", "toString": "b" }))));
}

#[test]
fn does_not_accept_every_value_for_a_type_named_like_an_object_prototype_member() {
    let validator = compile(&json!({ "type": "constructor" })).expect("the schema compiles");
    assert!(!validator.check(&Value::Number(1.0)));
}

#[test]
fn rejects_nan_and_infinity_as_a_number_or_an_integer() {
    let schemas = [
        json!({ "type": "number" }),
        json!({ "type": "integer" }),
        json!({ "type": "number", "minimum": 0 }),
        json!({ "type": ["number", "string"] }),
        json!({ "anyOf": [{ "type": "number" }, { "type": "null" }] }),
        json!({ "type": "object", "properties": { "a": { "type": "number" } } }),
    ];
    for schema in &schemas {
        let validator = compile(schema).expect("the schema compiles");
        for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let value = if schema.get("type").and_then(|value| value.as_str()) == Some("object") {
                Value::Object(
                    [("a".to_string(), Value::Number(number))]
                        .into_iter()
                        .collect(),
                )
            } else {
                Value::Number(number)
            };
            assert!(!validator.check(&value), "{schema} with {number}");
        }
    }
    let validator = compile(&json!({ "type": "number" })).expect("the schema compiles");
    assert_eq!(
        validator.errors(&Value::Number(f64::NAN)),
        vec![error("", "type", "expected number, got NaN")]
    );
}

#[test]
fn treats_format_as_an_annotation_unless_formats_are_given() {
    let schema = json!({ "type": "string", "format": "email" });
    assert!(
        compile(&schema)
            .expect("the schema compiles")
            .check(&value(&json!("not an email")))
    );
    let options = CompileOptions::new().with_formats(pi_rs_schema::formats::formats());
    let validator = compile_with(&schema, &options).expect("the schema compiles");
    assert!(!validator.check(&value(&json!("not an email"))));
    assert!(validator.check(&value(&json!("a@example.com"))));
}

#[test]
fn resolves_a_reference_to_a_registered_document() {
    let point =
        json!({ "type": "object", "properties": { "x": { "type": "number" } }, "required": ["x"] });
    let mut schemas = indexmap::IndexMap::new();
    schemas.insert("https://example.com/point.json".to_string(), point);
    let options = CompileOptions::new().with_schemas(schemas);
    let validator = compile_with(
        &json!({ "$ref": "https://example.com/point.json" }),
        &options,
    )
    .expect("the schema compiles");
    assert!(validator.check(&value(&json!({ "x": 1 }))));
    assert!(!validator.check(&value(&json!({}))));
}

#[test]
fn fails_for_a_reference_that_names_nothing() {
    let error = compile(&json!({ "$ref": "https://example.com/missing.json" }))
        .expect_err("the schema fails");
    assert!(
        matches!(&error, CompileError::UnresolvedRef(reference) if reference.contains("missing.json"))
    );
    let error = compile(&json!({ "$ref": "#/$defs/missing" })).expect_err("the schema fails");
    assert!(matches!(error, CompileError::UnresolvedRef(_)));
}

#[test]
fn applies_draft_seven_semantics_a_reference_overrides_its_siblings() {
    let schema = json!({
        "definitions": { "s": { "type": "string" } },
        "$ref": "#/definitions/s",
        "maxLength": 1
    });
    let options = CompileOptions::new().with_draft(Draft::V7);
    assert!(
        compile_with(&schema, &options)
            .expect("the schema compiles")
            .check(&value(&json!("ab")))
    );
    assert!(
        !compile(&schema)
            .expect("the schema compiles")
            .check(&value(&json!("ab")))
    );
}

#[test]
fn compiles_a_type_box_schema_as_it_is() {
    let schema = json!({
        "type": "object",
        "properties": { "name": { "type": "string" }, "tags": { "type": "array", "items": { "type": "string" } } }
    });
    let validator = compile(&schema).expect("the schema compiles");
    assert!(validator.check(&value(&json!({ "name": "a", "tags": [] }))));
    assert_eq!(
        validator.errors(&value(&json!({ "name": "a", "tags": [1] }))),
        vec![error("/tags/0", "type", "expected string, got integer")]
    );
}

#[test]
fn compiles_a_recursive_schema() {
    let tree = json!({
        "type": "object",
        "properties": { "value": { "type": "number" }, "children": { "type": "array", "items": { "$ref": "#" } } },
        "required": ["value"]
    });
    let validator = compile(&tree).expect("the schema compiles");
    assert!(validator.check(&value(
        &json!({ "value": 1, "children": [{ "value": 2, "children": [{ "value": 3 }] }] })
    )));
    assert_eq!(
        validator.errors(&value(
            &json!({ "value": 1, "children": [{ "value": 2, "children": [{}] }] })
        )),
        vec![error("/children/0/children/0/value", "required", "missing")]
    );
}

#[test]
fn applies_a_relative_root_id_once_when_resolving_local_references() {
    let schema = json!({
        "$id": "schemas/tool.json",
        "type": "object",
        "properties": { "count": { "$ref": "#/$defs/count" } },
        "required": ["count"],
        "$defs": { "count": { "type": "integer" } }
    });
    assert!(
        compile(&schema)
            .expect("the schema compiles")
            .check(&value(&json!({ "count": 2 })))
    );
    let options = CompileOptions::new().with_unresolved_refs(UnresolvedRefs::Reject);
    let validator = compile_with(&schema, &options).expect("the schema compiles");
    assert!(validator.check(&value(&json!({ "count": 2 }))));
    assert_eq!(
        validator.errors(&value(&json!({ "count": "a" }))),
        vec![error("/count", "type", "expected integer, got string")]
    );
    let coercing = CompileOptions::new().with_coerce(true);
    let validator = compile_with(&schema, &coercing).expect("the schema compiles");
    assert_eq!(
        validator.parse(&value(&json!({ "count": "2" }))),
        Ok(value(&json!({ "count": 2 })))
    );
}

#[test]
fn applies_a_relative_root_id_once_in_schemas_with_unevaluated_keywords() {
    let schema = json!({
        "$id": "schemas/tool.json",
        "type": "object",
        "properties": { "count": { "$ref": "#/$defs/count" } },
        "$defs": { "count": { "type": "integer" } },
        "unevaluatedProperties": false
    });
    let options = CompileOptions::new().with_unresolved_refs(UnresolvedRefs::Reject);
    let validator = compile_with(&schema, &options).expect("the schema compiles");
    assert!(validator.check(&value(&json!({ "count": 2 }))));
    assert_eq!(
        validator.errors(&value(&json!({ "count": 2, "extra": 1 }))),
        vec![error("/extra", "unevaluatedProperties", "unknown field")]
    );
    let items = json!({
        "$id": "schemas/list.json",
        "type": "array",
        "prefixItems": [{ "$ref": "#/$defs/count" }],
        "$defs": { "count": { "type": "integer" } },
        "unevaluatedItems": false
    });
    let options = CompileOptions::new().with_unresolved_refs(UnresolvedRefs::Reject);
    assert!(
        compile_with(&items, &options)
            .expect("the schema compiles")
            .check(&value(&json!([2])))
    );
    assert!(
        !compile(&items)
            .expect("the schema compiles")
            .check(&value(&json!([2, 3])))
    );
}

#[test]
fn explains_errors_under_nested_resources_with_a_relative_id() {
    let schema = json!({
        "$id": "https://example.com/root.json",
        "allOf": [{ "$ref": "#/$defs/child" }],
        "$defs": {
            "child": {
                "$id": "schemas/tool.json",
                "$ref": "#/$defs/props",
                "unevaluatedProperties": false,
                "$defs": { "props": { "properties": { "count": { "type": "integer" } } } }
            }
        }
    });
    let validator = compile(&schema).expect("the schema compiles");
    assert_eq!(
        validator.errors(&value(&json!({ "count": 2, "extra": 1 }))),
        vec![error("/extra", "unevaluatedProperties", "unknown field")]
    );
    let branch = json!({
        "$id": "https://example.com/root.json",
        "anyOf": [{ "$id": "a/b.json", "$ref": "#/$defs/n", "$defs": { "n": { "type": "integer" } } }, { "type": "boolean" }]
    });
    let validator = compile(&branch).expect("the schema compiles");
    assert_eq!(
        validator.errors(&value(&json!({}))),
        vec![error("", "type", "expected integer, got object")]
    );
}

#[test]
fn fails_for_a_malformed_schema() {
    let error = compile(&json!({ "properties": { "a": 1 } })).expect_err("the schema fails");
    assert!(matches!(error, CompileError::NotASchema(_)));
}

#[test]
fn rejects_only_values_at_their_place_with_unresolved_references_rejected() {
    let dangling =
        json!({ "type": "object", "properties": { "t": { "$ref": "#/definitions/Missing" } } });
    let external = json!({ "type": "object", "properties": { "t": { "$ref": "https://example.com/schemas/t.json" } } });
    for schema in [&dangling, &external] {
        let options = CompileOptions::new().with_unresolved_refs(UnresolvedRefs::Reject);
        let validator = compile_with(schema, &options).expect("the schema compiles");
        assert!(validator.check(&value(&json!({}))));
        assert!(!validator.check(&value(&json!({ "t": 1 }))));
    }
    let options = CompileOptions::new().with_unresolved_refs(UnresolvedRefs::Reject);
    let validator = compile_with(&dangling, &options).expect("the schema compiles");
    assert_eq!(
        validator.errors(&value(&json!({ "t": 1 }))),
        vec![error(
            "/t",
            "$ref",
            "unresolved reference #/definitions/Missing"
        )]
    );
}

#[test]
fn leaves_values_at_their_place_uncoerced() {
    let dangling =
        json!({ "type": "object", "properties": { "t": { "$ref": "#/definitions/Missing" } } });
    let options = CompileOptions::new()
        .with_unresolved_refs(UnresolvedRefs::Reject)
        .with_coerce(true);
    let validator = compile_with(&dangling, &options).expect("the schema compiles");
    assert_eq!(
        validator.parse(&value(&json!({ "t": "1" }))),
        Err(vec![error(
            "/t",
            "$ref",
            "unresolved reference #/definitions/Missing"
        )])
    );
}

#[test]
fn the_leaf_checks_agree_with_the_standalone_check() {
    let leaves: Vec<(JsonSchema, Vec<serde_json::Value>)> = vec![
        (
            json!({ "const": "a" }),
            vec![json!("a"), json!("b"), json!(1), json!(null)],
        ),
        (
            json!({ "const": null }),
            vec![json!(null), json!(0), json!(false), json!("null")],
        ),
        (
            json!({ "enum": ["a", 1, null] }),
            vec![
                json!("a"),
                json!(1),
                json!(null),
                json!("b"),
                json!(2),
                json!(true),
            ],
        ),
        (
            json!({ "type": ["string", "null"] }),
            vec![json!("a"), json!(null), json!(1), json!({}), json!([])],
        ),
        (
            json!({ "type": ["integer", "boolean"] }),
            vec![json!(1), json!(1.5), json!(true), json!("1")],
        ),
        (
            json!({ "type": ["number", "array"] }),
            vec![json!(1.5), json!(2), json!([]), json!({}), json!([0])],
        ),
        (
            json!({ "type": ["object", "bytes"] }),
            vec![json!({}), json!([0]), json!([]), json!(null)],
        ),
        (
            json!({ "anyOf": [{ "type": "string" }, { "type": "null" }] }),
            vec![json!("a"), json!(null), json!(1)],
        ),
        (
            json!({ "anyOf": [{ "type": "integer" }, { "type": "string", "minLength": 2 }] }),
            vec![json!(1), json!(1.5), json!("ab"), json!("a")],
        ),
        (
            json!({ "type": "integer", "minimum": 1, "exclusiveMaximum": 5 }),
            vec![
                json!(1),
                json!(4),
                json!(5),
                json!(0),
                json!(2.5),
                json!("2"),
            ],
        ),
        (
            json!({ "type": "number", "exclusiveMinimum": 0, "maximum": 1 }),
            vec![json!(0.5), json!(1), json!(0), json!(1.5), json!("0.5")],
        ),
        (
            json!({ "type": "integer", "multipleOf": 2 }),
            vec![json!(2), json!(3), json!(2.5)],
        ),
        (
            json!({ "type": "string", "minLength": 2, "maxLength": 3 }),
            vec![
                json!("ab"),
                json!("abc"),
                json!("a"),
                json!("abcd"),
                json!("😀😀"),
                json!(12),
            ],
        ),
        (
            json!({ "type": "string", "pattern": "^a+$" }),
            vec![json!("aa"), json!("ab"), json!(1)],
        ),
        (
            json!({ "minimum": 1 }),
            vec![json!(1), json!(0), json!("x"), json!(null)],
        ),
    ];
    for (schema, values) in &leaves {
        let alone = compile(schema).expect("the schema compiles");
        let closed = compile(&json!({ "type": "object", "properties": { "p": schema }, "additionalProperties": false }))
            .expect("the schema compiles");
        let open =
            compile(&json!({ "type": "object", "properties": { "p": schema }, "required": ["q"] }))
                .expect("the schema compiles");
        let items =
            compile(&json!({ "type": "array", "items": schema })).expect("the schema compiles");
        for entry in values {
            let expected = alone.check(&value(entry));
            let mut object = serde_json::Map::new();
            object.insert("p".to_string(), entry.clone());
            assert_eq!(
                closed.check(&value(&serde_json::Value::Object(object))),
                expected,
                "{schema} with {entry}"
            );
            let mut object = serde_json::Map::new();
            object.insert("p".to_string(), entry.clone());
            object.insert("q".to_string(), json!(1));
            assert_eq!(
                open.check(&value(&serde_json::Value::Object(object))),
                expected,
                "{schema} with {entry}"
            );
            assert_eq!(
                items.check(&value(&json!([entry]))),
                expected,
                "{schema} with {entry}"
            );
        }
    }
}

#[test]
fn finds_properties_in_any_order() {
    let validator = compile(&json!({
        "type": "object",
        "properties": { "a": { "const": 1 }, "b": { "type": "string" }, "c": { "type": "integer", "minimum": 0 } },
        "required": ["a", "c"],
        "additionalProperties": false
    }))
    .expect("the schema compiles");
    assert!(validator.check(&value(&json!({ "a": 1, "b": "x", "c": 0 }))));
    assert!(validator.check(&value(&json!({ "c": 0, "b": "x", "a": 1 }))));
    assert!(validator.check(&value(&json!({ "b": "x", "c": 0, "a": 1 }))));
    assert!(!validator.check(&value(&json!({ "a": 1, "c": 0, "a2": 1 }))));
    assert!(!validator.check(&value(&json!({ "c": -1, "a": 1 }))));
    assert!(!validator.check(&value(&json!({ "c": 0, "b": 1, "a": 1 }))));
    assert!(validator.check(&object(vec![
        ("c", Value::Number(0.0)),
        ("b", Value::Undefined),
        ("a", Value::Number(1.0)),
    ])));
    assert!(!validator.check(&value(&json!({ "c": 0, "b": "x" }))));
}

#[test]
fn counts_the_code_points_of_a_string() {
    // A pair of surrogate halves is one code point; a Rust string holds no unpaired surrogate.
    let validator =
        compile(&json!({ "type": "string", "minLength": 2 })).expect("the schema compiles");
    assert!(validator.check(&Value::String("😀😀".to_string())));
    assert!(!validator.check(&Value::String("😀".to_string())));
    let validator =
        compile(&json!({ "type": "string", "maxLength": 1 })).expect("the schema compiles");
    assert!(validator.check(&Value::String("😀".to_string())));
    assert!(!validator.check(&Value::String("😀😀".to_string())));
}

#[test]
fn lists_at_most_twenty_enum_values() {
    let values: Vec<serde_json::Value> =
        (0..5000).map(|index| json!(format!("v{index}"))).collect();
    let validator = compile(
        &json!({ "type": "object", "properties": { "e": { "type": "string", "enum": values } } }),
    )
    .expect("the schema compiles");
    assert!(validator.check(&value(&json!({ "e": "v4999" }))));
    let errors = validator.errors(&value(&json!({ "e": "v5000" })));
    assert_eq!(
        errors[0].message,
        "expected one of [\"v0\",\"v1\",\"v2\",\"v3\",\"v4\",\"v5\",\"v6\",\"v7\",\"v8\",\"v9\",\"v10\",\"v11\",\"v12\",\"v13\",\"v14\",\"v15\",\"v16\",\"v17\",\"v18\",\"v19\", ... and 4980 more]"
    );
    let validator = compile(&json!({ "enum": ["a", "b"] })).expect("the schema compiles");
    assert_eq!(
        validator.errors(&value(&json!("c"))),
        vec![error("", "enum", "expected one of [\"a\",\"b\"]")]
    );
}

#[test]
fn accepts_a_ten_thousand_value_enum() {
    let values: Vec<serde_json::Value> = (0..10_000)
        .map(|index| json!(format!("member{index}")))
        .collect();
    let validator = compile(&json!({ "enum": values })).expect("the schema compiles");
    assert!(validator.check(&value(&json!("member9999"))));
}

#[test]
fn describes_contains_bounds_in_words() {
    let message = |schema: serde_json::Value| {
        let mut schema = schema;
        schema
            .as_object_mut()
            .expect("the schema is an object")
            .insert("type".to_string(), json!("array"));
        let validator = compile(&schema).expect("the schema compiles");
        validator.errors(&value(&json!(["a"])))[0].message.clone()
    };
    assert_eq!(
        message(json!({ "contains": { "type": "number" } })),
        "expected at least 1 matching item"
    );
    assert_eq!(
        message(json!({ "contains": { "type": "number" }, "minContains": 2 })),
        "expected at least 2 matching items"
    );
    assert_eq!(
        message(json!({ "contains": { "type": "string" }, "maxContains": 0, "minContains": 0 })),
        "expected at most 0 matching items"
    );
    assert_eq!(
        message(json!({ "contains": { "type": "number" }, "minContains": 1, "maxContains": 3 })),
        "expected between 1 and 3 matching items"
    );
}

#[test]
fn blames_a_one_of_failure_on_the_branch_that_declares_most_of_the_properties() {
    let schema = json!({
        "type": "object",
        "properties": { "where": { "$ref": "#/$defs/F" } },
        "$defs": {
            "F": {
                "oneOf": [
                    {
                        "type": "object",
                        "properties": { "field": { "type": "string" }, "eq": { "type": "string" } },
                        "required": ["field", "eq"],
                        "additionalProperties": false
                    },
                    {
                        "type": "object",
                        "properties": { "and": { "type": "array", "items": { "$ref": "#/$defs/F" } } },
                        "required": ["and"],
                        "additionalProperties": false
                    }
                ]
            }
        }
    });
    let validator = compile(&schema).expect("the schema compiles");
    assert_eq!(
        validator.errors(&value(
            &json!({ "where": { "and": [{ "field": "a", "eq": 1, "x": 2 }] } })
        )),
        vec![
            error("/where/and/0/eq", "type", "expected string, got integer"),
            error("/where/and/0/x", "additionalProperties", "unknown field"),
        ]
    );
}

#[test]
fn explains_one_object_reached_through_the_same_reference_at_two_paths_at_each_path() {
    let schema = json!({
        "type": "object",
        "properties": { "a": { "$ref": "#/$defs/P" }, "b": { "$ref": "#/$defs/P" } },
        "$defs": { "P": { "type": "object", "properties": { "n": { "type": "integer" } } } }
    });
    let validator = compile(&schema).expect("the schema compiles");
    let shared = value(&json!({ "n": "x" }));
    assert_eq!(
        validator.errors(&Value::Object(
            [("a".to_string(), shared.clone()), ("b".to_string(), shared)]
                .into_iter()
                .collect()
        )),
        vec![
            error("/a/n", "type", "expected integer, got string"),
            error("/b/n", "type", "expected integer, got string"),
        ]
    );
}

#[test]
fn checks_the_extra_formats() {
    let options = CompileOptions::new().with_formats(pi_rs_schema::formats::formats());
    let validator = compile_with(&json!({ "type": "string", "format": "url" }), &options)
        .expect("the schema compiles");
    assert!(validator.check(&value(&json!("https://example.com/a?b=1"))));
    assert!(!validator.check(&value(&json!("not a url"))));
    let validator = compile_with(
        &json!({ "type": "string", "format": "json-pointer-uri-fragment" }),
        &options,
    )
    .expect("the schema compiles");
    assert!(validator.check(&value(&json!("#/a/b~1c"))));
    assert!(!validator.check(&value(&json!("#/a b"))));
    assert!(!validator.check(&value(&json!("/a"))));
}

#[test]
fn checks_the_bigint_type_with_its_bounds() {
    let schema = json!({ "type": "bigint", "minimum": "0" });
    let validator = compile(&schema).expect("the schema compiles");
    assert!(validator.check(&Value::BigInt(5.into())));
    assert!(validator.check(&Value::BigInt(0.into())));
    assert!(!validator.check(&Value::BigInt((-1).into())));
    assert!(!validator.check(&Value::Number(3.0)));
    let schema = json!({ "type": "bigint", "maximum": "10", "multipleOf": "5" });
    assert!(
        compile(&schema)
            .expect("the schema compiles")
            .check(&Value::BigInt(10.into()))
    );
    let schema = json!({ "type": "bigint", "multipleOf": "5" });
    assert!(
        !compile(&schema)
            .expect("the schema compiles")
            .check(&Value::BigInt(7.into()))
    );
    let schema = json!({ "type": "bigint", "exclusiveMaximum": "3" });
    assert!(
        !compile(&schema)
            .expect("the schema compiles")
            .check(&Value::BigInt(3.into()))
    );
    let validator =
        compile(&json!({ "type": "bigint", "minimum": "0" })).expect("the schema compiles");
    assert_eq!(
        validator.errors(&Value::BigInt((-2).into())),
        vec![error("", "minimum", "expected >= 0")]
    );
    let validator = compile(&json!({ "type": "bigint" })).expect("the schema compiles");
    assert_eq!(
        validator.errors(&Value::Number(2.0)),
        vec![error("", "type", "expected bigint, got integer")]
    );
}

#[test]
fn checks_the_undefined_type() {
    let validator = compile(&json!({ "type": "undefined" })).expect("the schema compiles");
    assert!(validator.check(&Value::Undefined));
    assert!(!validator.check(&Value::Null));
    let schema = json!({
        "type": "object",
        "properties": { "selection": { "const": "average" }, "principalId": { "type": "undefined" } },
        "required": ["selection", "principalId"]
    });
    let validator = compile(&schema).expect("the schema compiles");
    let with_undefined = object(vec![
        ("selection", Value::String("average".to_string())),
        ("principalId", Value::Undefined),
    ]);
    assert!(validator.check(&with_undefined));
    assert!(!validator.check(&value(&json!({ "selection": "average" }))));
    assert!(!validator.check(&value(
        &json!({ "selection": "average", "principalId": "x" })
    )));
    assert!(!validator.check(&value(
        &json!({ "selection": "average", "principalId": null })
    )));
    assert_eq!(
        validator.errors(&value(&json!({ "selection": "average" }))),
        vec![error("/principalId", "required", "missing")]
    );
    assert_eq!(
        validator.errors(&value(
            &json!({ "selection": "average", "principalId": "x" })
        )),
        vec![error(
            "/principalId",
            "type",
            "expected undefined, got string"
        )]
    );
}

#[test]
fn checks_a_required_property_set_to_undefined_by_its_schema() {
    for property in [
        json!({ "anyOf": [{ "type": "string" }, { "type": "undefined" }] }),
        json!({}),
        json!({ "type": "undefined" }),
    ] {
        for additional in [None, Some(json!(false))] {
            let mut schema =
                json!({ "type": "object", "properties": { "a": property }, "required": ["a"] });
            if let Some(additional) = additional {
                schema
                    .as_object_mut()
                    .expect("the schema is an object")
                    .insert("additionalProperties".to_string(), additional);
            }
            let validator = compile(&schema).expect("the schema compiles");
            let with_undefined = object(vec![("a", Value::Undefined)]);
            assert!(validator.check(&with_undefined));
            assert!(!validator.check(&value(&json!({}))));
        }
    }
    let validator = compile(&json!({ "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"] }))
        .expect("the schema compiles");
    let with_undefined = object(vec![("name", Value::Undefined)]);
    assert!(!validator.check(&with_undefined));
    assert_eq!(
        validator.errors(&with_undefined),
        vec![error("/name", "required", "missing")]
    );
    let validator =
        compile(&json!({ "type": "object", "properties": { "name": { "type": "string" } } }))
            .expect("the schema compiles");
    assert!(validator.check(&object(vec![("name", Value::Undefined)])));
}
