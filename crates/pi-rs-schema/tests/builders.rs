// The builders: the JSON Schema they produce and how they compile.

use pi_rs_schema::builders::*;
use pi_rs_schema::{Value, compile};
use serde_json::json;

#[test]
fn produces_plain_json_schema() {
    let schema = obj(
        &Properties::new()
            .with(
                "path",
                &str_with(
                    &StrOptions::new()
                        .annotations(Annotations::new().description("File path"))
                        .bounds(LengthBounds::new().min_length(1)),
                ),
            )
            .with_optional(
                "offset",
                &opt(&int(
                    &NumOptions::new().bounds(NumberBounds::new().minimum(1.0))
                )),
            )
            .with(
                "mode",
                &one_of(&[json!("read"), json!("write")], &Annotations::new()),
            ),
        &Annotations::new(),
        false,
    );
    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path", "minLength": 1 },
                "offset": { "type": "integer", "minimum": 1 },
                "mode": { "enum": ["read", "write"] }
            },
            "required": ["path", "mode"],
            "additionalProperties": false
        })
    );
}

#[test]
fn produces_every_leaf() {
    assert_eq!(str(), json!({ "type": "string" }));
    assert_eq!(num(&NumOptions::new()), json!({ "type": "number" }));
    assert_eq!(int(&NumOptions::new()), json!({ "type": "integer" }));
    assert_eq!(bool(&Annotations::new()), json!({ "type": "boolean" }));
    assert_eq!(nul(&Annotations::new()), json!({ "type": "null" }));
    assert_eq!(unknown(&Annotations::new()), json!({}));
    assert_eq!(lit(json!(3), &Annotations::new()), json!({ "const": 3 }));
    assert_eq!(
        bytes(&Annotations::new(), &LengthBounds::new().max_length(8)),
        json!({ "type": "bytes", "maxLength": 8 })
    );
    assert_eq!(
        arr(
            &str(),
            &ArrOptions::new().bounds(LengthBounds::new().max_length(2))
        ),
        json!({ "type": "array", "items": { "type": "string" }, "maxLength": 2 })
    );
    assert_eq!(
        map(&num(&NumOptions::new()), &Annotations::new()),
        json!({ "type": "object", "additionalProperties": { "type": "number" } })
    );
    assert_eq!(
        tuple(&[str(), num(&NumOptions::new())], &Annotations::new()),
        json!({
            "type": "array",
            "prefixItems": [{ "type": "string" }, { "type": "number" }],
            "items": false,
            "minItems": 2
        })
    );
    assert_eq!(
        nullable(&str(), &Annotations::new()),
        json!({ "anyOf": [{ "type": "string" }, { "type": "null" }] })
    );
    assert_eq!(
        obj(
            &Properties::new().with("a", &str()),
            &Annotations::new(),
            true
        ),
        json!({ "type": "object", "properties": { "a": { "type": "string" } }, "required": ["a"] })
    );
}

#[test]
fn builds_a_bigint_with_bounds() {
    let metric = nullable(
        &bigint(&BigintOptions::new().bounds(BigintBounds::new().minimum("0"))),
        &Annotations::new(),
    );
    assert_eq!(
        metric,
        json!({ "anyOf": [{ "type": "bigint", "minimum": "0" }, { "type": "null" }] })
    );
    let validator = compile(&metric).expect("the schema compiles");
    assert!(validator.check(&Value::BigInt(3.into())));
    assert!(!validator.check(&Value::BigInt((-1).into())));
    assert!(!validator.check(&Value::Number(3.0)));
}

#[test]
fn types_a_schema_as_given_with_unsafe() {
    let usage = unsafe_schema(&json!({}));
    let level = unsafe_schema(&json!({ "type": "string", "enum": ["low", "high"] }));
    assert_eq!(usage, json!({}));
    assert_eq!(level, json!({ "type": "string", "enum": ["low", "high"] }));
    let status = obj(
        &Properties::new()
            .with("usage", &nullable(&usage, &Annotations::new()))
            .with("level", &level),
        &Annotations::new(),
        false,
    );
    // Nothing checks the given type: an unconstrained schema accepts any value.
    assert!(
        compile(&usage)
            .expect("the schema compiles")
            .check(&Value::String("not usage".to_string()))
    );
    assert!(
        !compile(&level)
            .expect("the schema compiles")
            .check(&Value::String("medium".to_string()))
    );
    assert_eq!(status.get("type"), Some(&json!("object")));
}

#[test]
fn builds_a_recursive_schema_that_checks_every_level() {
    let tree = recursive("Tree", |self_reference| {
        obj(
            &Properties::new()
                .with("name", &str())
                .with("children", &arr(self_reference, &ArrOptions::new())),
            &Annotations::new(),
            false,
        )
    });
    assert_eq!(
        tree,
        json!({
            "type": "object",
            "properties": { "name": { "type": "string" }, "children": { "type": "array", "items": { "$ref": "Tree" } } },
            "required": ["name", "children"],
            "additionalProperties": false,
            "$id": "Tree"
        })
    );
    let validator = compile(&tree).expect("the schema compiles");
    assert!(validator.check(&Value::from(
        &json!({ "name": "a", "children": [{ "name": "b", "children": [] }] })
    )));
    assert_eq!(
        validator.errors(&Value::from(&json!({ "name": "a", "children": [{ "name": "b", "children": [{ "name": 1, "children": [] }] }] }))),
        vec![pi_rs_schema::ValidationError::new(
            "/children/0/children/0/name",
            "type",
            "expected string, got integer"
        )]
    );
}

#[test]
fn nests_recursive_schemas_with_distinct_ids_inside_other_schemas() {
    let tree = recursive("Tree", |self_reference| {
        obj(
            &Properties::new()
                .with("name", &str())
                .with("children", &arr(self_reference, &ArrOptions::new())),
            &Annotations::new(),
            false,
        )
    });
    let list = recursive("List", |self_reference| {
        obj(
            &Properties::new()
                .with("value", &num(&NumOptions::new()))
                .with("next", &nullable(self_reference, &Annotations::new())),
            &Annotations::new(),
            false,
        )
    });
    let both = obj(
        &Properties::new()
            .with("tree", &tree)
            .with_optional("list", &opt(&list)),
        &Annotations::new(),
        false,
    );
    let validator = compile(&both).expect("the schema compiles");
    let root = json!({ "name": "a", "children": [{ "name": "b", "children": [] }] });
    assert!(validator.check(&Value::from(
        &json!({ "tree": root, "list": { "value": 1, "next": { "value": 2, "next": null } } })
    )));
    assert!(!validator.check(&Value::from(
        &json!({ "tree": root, "list": { "value": 1, "next": { "value": "2", "next": null } } })
    )));
    assert!(!validator.check(&Value::from(&json!({ "tree": { "name": "a", "children": [{ "name": "b", "children": [{ "name": 3 }] }] } }))));
}

#[test]
fn compiles_into_checks_with_the_declared_semantics() {
    let schema = obj(
        &Properties::new()
            .with(
                "name",
                &str_with(&StrOptions::new().bounds(LengthBounds::new().min_length(1))),
            )
            .with_optional(
                "tags",
                &opt(&arr(&str(), &ArrOptions::new().unique_items(true))),
            ),
        &Annotations::new(),
        false,
    );
    let validator = compile(&schema).expect("the schema compiles");
    assert!(validator.check(&Value::from(&json!({ "name": "a" }))));
    assert!(validator.check(&Value::from(&json!({ "name": "a", "tags": ["x", "y"] }))));
    assert_eq!(
        validator.errors(&Value::from(
            &json!({ "name": "", "tags": ["x", "x"], "other": 1 })
        )),
        vec![
            pi_rs_schema::ValidationError::new(
                "/name",
                "minLength",
                "expected at least 1 characters"
            ),
            pi_rs_schema::ValidationError::new("/tags", "uniqueItems", "expected unique items"),
            pi_rs_schema::ValidationError::new("/other", "additionalProperties", "unknown field"),
        ]
    );
}
