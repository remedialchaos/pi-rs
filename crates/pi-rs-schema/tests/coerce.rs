// compile(schema, { coerce: true }): coerce() and parse(), the three coercion passes (optional nulls, TypeBox's
// conversion rules, primitive coercion by type), their order where they conflict, and the guarantees around them.

use pi_rs_schema::{CompileOptions, JsonSchema, Value, compile_with};
use serde_json::json;

/// The coerced value of `value` under an object schema with one required property `value`.
fn coerce_value(schema: &JsonSchema, value: &Value) -> Value {
    let root =
        json!({ "type": "object", "properties": { "value": schema }, "required": ["value"] });
    let validator =
        compile_with(&root, &CompileOptions::new().with_coerce(true)).expect("the schema compiles");
    let coerced = validator
        .coerce(&Value::Object(
            [("value".to_string(), value.clone())].into_iter().collect(),
        ))
        .expect("the validator coerces");
    match coerced {
        Value::Object(mut members) => members.shift_remove("value").unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn coerce(schema: &JsonSchema, value: &Value) -> Value {
    let validator = compile_with(schema, &CompileOptions::new().with_coerce(true))
        .expect("the schema compiles");
    validator.coerce(value).expect("the validator coerces")
}

fn json_value(value: &serde_json::Value) -> Value {
    Value::from(value)
}

#[test]
fn never_modifies_the_value() {
    let value = json_value(&json!({ "count": "42", "tags": "a" }));
    let schema = json!({ "type": "object", "properties": { "count": { "type": "number" }, "tags": { "type": "array", "items": { "type": "string" } } } });
    let coerced = coerce(&schema, &value);
    assert_eq!(coerced, json_value(&json!({ "count": 42, "tags": ["a"] })));
    assert_eq!(value, json_value(&json!({ "count": "42", "tags": "a" })));
}

#[test]
fn returns_the_value_itself_when_nothing_changes() {
    let value = json_value(&json!({ "a": { "b": [1, 2] }, "c": "x" }));
    let schema = json!({ "type": "object", "properties": { "a": { "type": "object", "properties": { "b": { "type": "array", "items": { "type": "number" } } } }, "c": { "type": "string" } } });
    assert_eq!(coerce(&schema, &value), value);
}

#[test]
fn copies_only_the_changed_paths() {
    let value = json_value(&json!({ "a": { "b": [1, 2] }, "c": { "d": "1" } }));
    let schema = json!({ "type": "object", "properties": { "a": { "type": "object", "properties": { "b": { "type": "array", "items": { "type": "number" } } } }, "c": { "type": "object", "properties": { "d": { "type": "number" } } } } });
    let coerced = coerce(&schema, &value);
    assert_eq!(
        coerced,
        json_value(&json!({ "a": { "b": [1, 2] }, "c": { "d": 1 } }))
    );
    assert_eq!(
        value,
        json_value(&json!({ "a": { "b": [1, 2] }, "c": { "d": "1" } }))
    );
}

#[test]
fn leaves_the_values_it_cannot_coerce_for_the_check_to_report() {
    let schema = json!({ "type": "object", "properties": { "count": { "type": "number" } } });
    let validator = compile_with(&schema, &CompileOptions::new().with_coerce(true))
        .expect("the schema compiles");
    let coerced = validator
        .coerce(&json_value(&json!({ "count": "many" })))
        .expect("the validator coerces");
    assert_eq!(coerced, json_value(&json!({ "count": "many" })));
    assert!(!validator.check(&coerced));
}

#[test]
fn parse_returns_the_coerced_value_when_it_is_valid() {
    let schema = json!({ "type": "object", "properties": { "count": { "type": "number" }, "tags": { "type": "array", "items": { "type": "string" } } } });
    let validator = compile_with(&schema, &CompileOptions::new().with_coerce(true))
        .expect("the schema compiles");
    assert_eq!(
        validator.parse(&json_value(&json!({ "count": "42", "tags": "a" }))),
        Ok(json_value(&json!({ "count": 42, "tags": ["a"] })))
    );
}

#[test]
fn parse_returns_the_errors_of_the_coerced_value_when_it_is_invalid() {
    let schema = json!({ "type": "object", "properties": { "count": { "type": "number" }, "name": { "type": "string" } } });
    let validator = compile_with(&schema, &CompileOptions::new().with_coerce(true))
        .expect("the schema compiles");
    let errors = validator.parse(&json_value(&json!({ "count": "many", "name": 7 })));
    assert_eq!(
        errors,
        Err(vec![pi_rs_schema::ValidationError::new(
            "/count",
            "type",
            "expected number, got string"
        )])
    );
}

#[test]
fn validate_checks_without_coercing() {
    let schema = json!({ "type": "object", "properties": { "count": { "type": "number" }, "tags": { "type": "array", "items": { "type": "string" } } } });
    let value = json_value(&json!({ "count": 42, "tags": ["a"] }));
    for coercing in [false, true] {
        let validator = compile_with(&schema, &CompileOptions::new().with_coerce(coercing))
            .expect("the schema compiles");
        assert_eq!(validator.validate(&value), Ok(value.clone()));
        let errors = validator.validate(&json_value(&json!({ "count": "42", "tags": "a" })));
        assert_eq!(
            errors,
            Err(vec![
                pi_rs_schema::ValidationError::new("/count", "type", "expected number, got string"),
                pi_rs_schema::ValidationError::new("/tags", "type", "expected array, got string"),
            ])
        );
    }
}

#[test]
fn converts_to_a_bigint_like_type_box() {
    let schema = json!({ "type": "bigint" });
    assert_eq!(
        coerce_value(&schema, &Value::String("12n".to_string())),
        Value::BigInt(12.into())
    );
    assert_eq!(
        coerce_value(&schema, &Value::String("12.9".to_string())),
        Value::BigInt(12.into())
    );
    assert_eq!(
        coerce_value(&schema, &Value::Number(3.7)),
        Value::BigInt(3.into())
    );
    assert_eq!(
        coerce_value(&schema, &Value::Bool(true)),
        Value::BigInt(1.into())
    );
    assert_eq!(
        coerce_value(&schema, &Value::String("x".to_string())),
        Value::String("x".to_string())
    );
}

#[test]
fn converts_to_undefined_like_type_box() {
    let schema = json!({ "type": "undefined" });
    for value in [
        Value::Null,
        Value::Number(0.0),
        Value::BigInt(0.into()),
        Value::Bool(false),
        Value::String(String::new()),
        Value::String("0".to_string()),
        Value::String("null".to_string()),
        Value::String("undefined".to_string()),
    ] {
        assert_eq!(coerce_value(&schema, &value), Value::Undefined, "{value:?}");
    }
    assert_eq!(
        coerce_value(&schema, &Value::String("x".to_string())),
        Value::String("x".to_string())
    );
}

#[test]
fn drops_null_in_optional_non_nullable_properties() {
    let schema = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "offset": { "type": "number" },
            "nullable": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
            "metadata": { "type": "object", "properties": { "enabled": { "type": "boolean" } } }
        },
        "required": ["path"]
    });
    let value = json_value(
        &json!({ "path": "file.txt", "offset": null, "nullable": null, "metadata": { "enabled": null } }),
    );
    assert_eq!(
        coerce(&schema, &value),
        json_value(&json!({ "path": "file.txt", "nullable": null, "metadata": {} }))
    );
}

#[test]
fn keeps_a_null_whose_property_is_a_reference() {
    let schema = json!({
        "type": "object",
        "properties": { "value": { "$ref": "#/$defs/value" } },
        "$defs": { "value": { "anyOf": [{ "type": "number" }, { "type": "null" }] } }
    });
    assert_eq!(
        coerce(&schema, &json_value(&json!({ "value": null }))),
        json_value(&json!({ "value": null }))
    );
}

#[test]
fn walks_array_items() {
    let schema = json!({ "type": "array", "items": { "type": "object", "properties": { "a": { "type": "string" }, "b": { "type": "integer" } } } });
    assert_eq!(
        coerce(&schema, &json_value(&json!([{ "a": "x", "b": null }]))),
        json_value(&json!([{ "a": "x" }]))
    );
}

#[test]
fn converts_the_primitives_by_type() {
    assert_eq!(
        coerce_value(
            &json!({ "type": "number" }),
            &Value::String("42".to_string())
        ),
        Value::Number(42.0)
    );
    assert_eq!(
        coerce_value(&json!({ "type": "number" }), &Value::String(String::new())),
        Value::Number(0.0)
    );
    assert_eq!(
        coerce_value(
            &json!({ "type": "number" }),
            &Value::String("true".to_string())
        ),
        Value::Number(1.0)
    );
    assert_eq!(
        coerce_value(
            &json!({ "type": "integer" }),
            &Value::String("3.5".to_string())
        ),
        Value::Number(3.0)
    );
    assert_eq!(
        coerce_value(&json!({ "type": "integer" }), &Value::Number(1.5)),
        Value::Number(1.0)
    );
    assert_eq!(
        coerce_value(
            &json!({ "type": "boolean" }),
            &Value::String("1".to_string())
        ),
        Value::Bool(true)
    );
    assert_eq!(
        coerce_value(
            &json!({ "type": "boolean" }),
            &Value::String("FALSE".to_string())
        ),
        Value::Bool(false)
    );
    assert_eq!(
        coerce_value(
            &json!({ "type": "null" }),
            &Value::String("null".to_string())
        ),
        Value::Null
    );
    assert_eq!(
        coerce_value(&json!({ "type": "string" }), &Value::Number(42.0)),
        Value::String("42".to_string())
    );
}

#[test]
fn wraps_a_single_value_into_an_array_and_converts_the_items() {
    assert_eq!(
        coerce_value(
            &json!({ "type": "array", "items": { "type": "number" } }),
            &Value::String("1".to_string())
        ),
        json_value(&json!([1]))
    );
    assert_eq!(
        coerce_value(
            &json!({ "type": "array", "items": { "type": "string" } }),
            &Value::Null
        ),
        json_value(&json!(["null"]))
    );
}

#[test]
fn converts_tuple_items_in_place_and_never_wraps_for_a_tuple() {
    let tuple = json!({ "type": "array", "prefixItems": [{ "type": "number" }, { "type": "boolean" }], "items": false });
    assert_eq!(
        coerce_value(&tuple, &json_value(&json!(["1", "true"]))),
        json_value(&json!([1, true]))
    );
    assert_eq!(
        coerce_value(&tuple, &Value::String("1".to_string())),
        Value::String("1".to_string())
    );
}

#[test]
fn converts_const_and_enum_values_only_when_the_result_is_a_member() {
    assert_eq!(
        coerce_value(&json!({ "const": 1 }), &Value::String("1".to_string())),
        Value::Number(1.0)
    );
    assert_eq!(
        coerce_value(&json!({ "const": 1 }), &Value::String("2".to_string())),
        Value::String("2".to_string())
    );
    assert_eq!(
        coerce_value(
            &json!({ "enum": [1, 2, "hello"] }),
            &Value::String("2".to_string())
        ),
        Value::Number(2.0)
    );
    assert_eq!(
        coerce_value(
            &json!({ "enum": [1, 2, "hello"] }),
            &Value::String("world".to_string())
        ),
        Value::String("world".to_string())
    );
}

#[test]
fn keeps_a_value_one_union_branch_accepts() {
    let union = json!({ "anyOf": [{ "type": "number" }, { "type": "null" }] });
    assert_eq!(coerce_value(&union, &Value::Null), Value::Null);
    assert_eq!(
        coerce_value(&union, &Value::String("42".to_string())),
        Value::Number(42.0)
    );
    assert_eq!(
        coerce_value(&union, &Value::String("null".to_string())),
        Value::Null
    );
    assert_eq!(
        coerce_value(
            &json!({ "oneOf": [{ "type": "boolean" }, { "type": "number" }] }),
            &Value::String("1".to_string())
        ),
        Value::Bool(true)
    );
}

#[test]
fn converts_all_of_with_each_member_in_turn() {
    let schema = json!({
        "allOf": [
            { "type": "object", "properties": { "a": { "type": "number" } } },
            { "type": "object", "properties": { "b": { "type": "boolean" } } }
        ]
    });
    assert_eq!(
        coerce(&schema, &json_value(&json!({ "a": "1", "b": "true" }))),
        json_value(&json!({ "a": 1, "b": true }))
    );
}

#[test]
fn follows_a_reference() {
    let schema = json!({
        "type": "object",
        "properties": { "item": { "$ref": "#/$defs/item" } },
        "$defs": { "item": { "type": "object", "properties": { "id": { "type": "integer" } } } }
    });
    assert_eq!(
        coerce(&schema, &json_value(&json!({ "item": { "id": "7" } }))),
        json_value(&json!({ "item": { "id": 7 } }))
    );
}

#[test]
fn converts_pattern_properties_and_additional_properties() {
    let schema = json!({
        "type": "object",
        "properties": { "a": { "type": "number" } },
        "patternProperties": { "^n_": { "type": "number" } },
        "additionalProperties": { "type": "boolean" }
    });
    assert_eq!(
        coerce(
            &schema,
            &json_value(&json!({ "a": "1", "n_x": "2", "other": "true" }))
        ),
        json_value(&json!({ "a": 1, "n_x": 2, "other": true }))
    );
}

#[test]
fn coerces_like_ajv() {
    let cases: Vec<(JsonSchema, Value, Value)> = vec![
        (
            json!({ "type": "number" }),
            Value::Bool(true),
            Value::Number(1.0),
        ),
        (json!({ "type": "number" }), Value::Null, Value::Number(0.0)),
        (
            json!({ "type": "integer" }),
            Value::String("42".to_string()),
            Value::Number(42.0),
        ),
        (
            json!({ "type": "boolean" }),
            Value::String("true".to_string()),
            Value::Bool(true),
        ),
        (
            json!({ "type": "boolean" }),
            Value::Number(0.0),
            Value::Bool(false),
        ),
        (
            json!({ "type": "string" }),
            Value::Bool(true),
            Value::String("true".to_string()),
        ),
        (
            json!({ "type": "null" }),
            Value::String(String::new()),
            Value::Null,
        ),
        (json!({ "type": "null" }), Value::Number(0.0), Value::Null),
        (json!({ "type": "null" }), Value::Bool(false), Value::Null),
    ];
    for (schema, input, expected) in cases {
        assert_eq!(
            coerce_value(&schema, &input),
            expected,
            "{schema} with {input:?}"
        );
    }
}

#[test]
fn leaves_a_value_that_matches_one_of_a_type_arrays_types() {
    assert_eq!(
        coerce_value(
            &json!({ "type": ["number", "string"] }),
            &Value::String("1".to_string())
        ),
        Value::String("1".to_string())
    );
    assert_eq!(
        coerce_value(
            &json!({ "type": ["boolean", "number"] }),
            &Value::String("1".to_string())
        ),
        Value::Number(1.0)
    );
    assert_eq!(
        coerce_value(
            &json!({ "type": ["integer", "null"] }),
            &Value::String("5".to_string())
        ),
        Value::Number(5.0)
    );
}

#[test]
fn accepts_null_for_nullable_array_schemas_with_items() {
    assert_eq!(
        coerce_value(
            &json!({ "type": ["array", "null"], "items": { "type": "string" } }),
            &Value::Null
        ),
        Value::Null
    );
}

#[test]
fn coerces_nullable_unions_when_the_value_matches_no_branch() {
    assert_eq!(
        coerce_value(
            &json!({ "anyOf": [{ "type": "number" }, { "type": "null" }] }),
            &Value::String("42".to_string())
        ),
        Value::Number(42.0)
    );
    assert_eq!(
        coerce_value(
            &json!({ "oneOf": [{ "type": "number" }, { "type": "null" }] }),
            &Value::Null
        ),
        Value::Null
    );
}

#[test]
fn leaves_the_values_no_rule_coerces() {
    let schema = json!({ "type": "object", "properties": { "value": { "type": "integer" } }, "required": ["value"] });
    let validator = compile_with(&schema, &CompileOptions::new().with_coerce(true))
        .expect("the schema compiles");
    for input in [
        Value::String("abc".to_string()),
        Value::String("4x".to_string()),
        Value::Object(Default::default()),
        Value::Array(vec![
            Value::String("1".to_string()),
            Value::String("2".to_string()),
        ]),
    ] {
        let coerced = validator.coerce(&Value::Object(
            [("value".to_string(), input.clone())].into_iter().collect(),
        ));
        let coerced = coerced.expect("the validator coerces");
        assert!(!validator.check(&coerced), "{input:?}");
    }
}

#[test]
fn the_conversion_rules_win_where_the_passes_conflict() {
    assert_eq!(
        coerce_value(&json!({ "type": "string" }), &Value::Null),
        Value::String("null".to_string())
    );
    assert_eq!(
        coerce_value(&json!({ "type": "number" }), &Value::String(String::new())),
        Value::Number(0.0)
    );
    assert_eq!(
        coerce_value(
            &json!({ "type": "boolean" }),
            &Value::String("0".to_string())
        ),
        Value::Bool(false)
    );
    assert_eq!(
        coerce_value(
            &json!({ "type": "integer" }),
            &Value::String("42.1".to_string())
        ),
        Value::Number(42.0)
    );
    let schema = json!({ "type": "object", "properties": { "name": { "type": "string" } } });
    assert_eq!(
        coerce(&schema, &json_value(&json!({ "name": null }))),
        json_value(&json!({}))
    );
}

#[test]
fn converts_through_a_recursive_union_by_its_first_accepting_branch() {
    let tree = json!({
        "type": "object",
        "properties": { "t": { "$ref": "#/$defs/Tree" } },
        "$defs": { "Tree": { "anyOf": [{ "type": "string" }, { "type": "array", "items": { "$ref": "#/$defs/Tree" } }] } }
    });
    let validator =
        compile_with(&tree, &CompileOptions::new().with_coerce(true)).expect("the schema compiles");
    assert_eq!(
        validator.parse(&json_value(&json!({ "t": ["a", ["b"]] }))),
        Ok(json_value(&json!({ "t": ["a", ["b"]] })))
    );
    assert_eq!(
        validator.parse(&json_value(&json!({ "t": 5 }))),
        Ok(json_value(&json!({ "t": "5" })))
    );
    assert_eq!(
        validator.parse(&json_value(&json!({ "t": [5] }))),
        Ok(json_value(&json!({ "t": ["5"] })))
    );
}

#[test]
fn leaves_a_value_a_recursive_schema_cannot_convert_as_it_is() {
    let tree = json!({
        "type": "object",
        "properties": { "t": { "$ref": "#/$defs/Tree" } },
        "$defs": { "Tree": { "anyOf": [{ "type": "string" }, { "type": "array", "items": { "$ref": "#/$defs/Tree" } }] } }
    });
    let validator =
        compile_with(&tree, &CompileOptions::new().with_coerce(true)).expect("the schema compiles");
    assert!(validator.parse(&json_value(&json!({ "t": {} }))).is_err());
    assert_eq!(
        validator.coerce(&json_value(&json!({ "t": { "a": 1 } }))),
        Some(json_value(&json!({ "t": { "a": 1 } })))
    );
}

#[test]
fn converts_a_scalar_or_array_schema_that_refers_to_itself() {
    let schema = json!({
        "type": "object",
        "properties": {
            "value": { "anyOf": [{ "type": "number" }, { "type": "array", "items": { "$ref": "#/properties/value" } }] }
        },
        "required": ["value"]
    });
    let validator = compile_with(&schema, &CompileOptions::new().with_coerce(true))
        .expect("the schema compiles");
    assert_eq!(
        validator.parse(&json_value(&json!({ "value": "42" }))),
        Ok(json_value(&json!({ "value": 42 })))
    );
    assert_eq!(
        validator.parse(&json_value(&json!({ "value": ["1", ["2"]] }))),
        Ok(json_value(&json!({ "value": [1, [2]] })))
    );
    assert!(
        validator
            .parse(&json_value(&json!({ "value": {} })))
            .is_err()
    );
}

#[test]
fn resets_the_re_entry_state_between_calls_and_leaves_the_input_unchanged() {
    let schema =
        json!({ "anyOf": [{ "type": "number" }, { "type": "array", "items": { "$ref": "#" } }] });
    let validator = compile_with(&schema, &CompileOptions::new().with_coerce(true))
        .expect("the schema compiles");
    for _ in 0..20 {
        assert!(validator.parse(&json_value(&json!({}))).is_err());
        assert_eq!(
            validator.parse(&Value::String("42".to_string())),
            Ok(Value::Number(42.0))
        );
        let input = json_value(&json!([1, "2", ["3"]]));
        assert_eq!(validator.parse(&input), Ok(json_value(&json!([1, 2, [3]]))));
        assert_eq!(input, json_value(&json!([1, "2", ["3"]])));
    }
}

#[test]
fn converts_a_deep_value_every_branch_of_a_recursive_union_rejects() {
    let schema = json!({
        "type": "object",
        "properties": { "s": { "$ref": "#/$defs/S" } },
        "$defs": {
            "S": {
                "anyOf": [
                    { "type": "object", "additionalProperties": { "$ref": "#/$defs/S" }, "required": ["a"] },
                    { "type": "object", "additionalProperties": { "$ref": "#/$defs/S" }, "required": ["b"] }
                ]
            }
        }
    });
    let validator = compile_with(&schema, &CompileOptions::new().with_coerce(true))
        .expect("the schema compiles");
    let mut value = json_value(&json!({ "x": 1 }));
    for _ in 0..40 {
        value = Value::Object([("c".to_string(), value)].into_iter().collect());
    }
    let errors = validator.parse(&Value::Object(
        [("s".to_string(), value)].into_iter().collect(),
    ));
    let errors = errors.expect_err("the value is invalid");
    assert_eq!(errors.len(), 42);
    for (index, error) in errors.iter().take(41).enumerate() {
        assert_eq!(error.path, format!("/s{}/a", "/c".repeat(index)));
        assert_eq!(error.keyword, "required");
    }
    assert_eq!(errors[41].path, format!("/s{}/x", "/c".repeat(40)));
    assert_eq!(errors[41].keyword, "type");
}
