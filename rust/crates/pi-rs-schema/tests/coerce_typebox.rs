// TypeBox 1.3.27's Value.Convert tests (test/typebox/runtime/value/convert, MIT, (c) Haydn Paterson), ported to
// the conversion pass of pi-schema's coercion. The schemas are the JSON Schema TypeBox builds; the port reads
// only their JSON structure, never TypeBox's kind marks.
//
// The cases were recorded from the upstream TypeScript test file: each case is a schema, a value and the value
// the conversion produces.

use pi_rs_schema::{CompileOptions, Value, compile_coercing};
use serde_json::Value as Json;

/// One recorded case.
#[derive(Debug, serde::Deserialize)]
struct Case {
    schema: Json,
    value: Json,
    expected: Json,
}

/// Decodes a recorded value: a bigint and an undefined have no JSON form.
fn decode(value: &Json) -> Value {
    match value {
        Json::Object(map) => {
            if let Some(bigint) = map.get("$bigint").and_then(Json::as_str) {
                return Value::BigInt(bigint.parse().expect("the recorded bigint is a number"));
            }
            if map.get("$undefined").is_some() {
                return Value::Undefined;
            }
            Value::Object(
                map.iter()
                    .map(|(key, item)| (key.clone(), decode(item)))
                    .collect(),
            )
        }
        Json::Array(items) => Value::Array(items.iter().map(decode).collect()),
        other => Value::from(other),
    }
}

/// The conversion pass alone, as TypeBox's `Value.Convert`.
fn convert(schema: &Json, value: &Value) -> Value {
    let validator = compile_coercing(schema, &CompileOptions::new()).expect("the schema compiles");
    validator.convert(value)
}

#[test]
fn the_typebox_conversion_cases_pass() {
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("fixtures/typebox-convert-cases.json"))
            .expect("the cases are valid JSON");
    assert!(cases.len() > 200, "only {} cases ran", cases.len());
    let mut failures: Vec<String> = Vec::new();
    for case in &cases {
        let value = decode(&case.value);
        let expected = decode(&case.expected);
        let actual = convert(&case.schema, &value);
        if actual != expected {
            failures.push(format!(
                "schema {} with value {}: expected {}, got {actual:?}",
                case.schema, case.value, case.expected,
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} cases failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
