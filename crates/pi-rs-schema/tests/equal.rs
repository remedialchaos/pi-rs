// equal(): JSON equality, a drop-in for TypeBox's Value.Equal.

use pi_rs_schema::{Value, equal};
use serde_json::json;

/// The value of a JSON document.
fn value(json: &serde_json::Value) -> Value {
    Value::from(json)
}

#[test]
fn compares_json_values_structurally() {
    let cases: Vec<(serde_json::Value, serde_json::Value, bool)> = vec![
        (json!(1), json!(1), true),
        (json!("a"), json!("b"), false),
        (json!(null), json!(null), true),
        (json!(null), json!({ "$undefined": true }), false),
        (
            json!({ "a": 1, "b": [1, { "c": 2 }] }),
            json!({ "b": [1, { "c": 2 }], "a": 1 }),
            true,
        ),
        (json!({ "a": 1 }), json!({ "a": 1, "b": 2 }), false),
        (json!([1, 2]), json!([2, 1]), false),
        (json!([1]), json!({ "0": 1 }), false),
        (json!({}), json!([]), false),
    ];
    for (left, right, expected) in &cases {
        assert_eq!(
            equal(&value(left), &value(right)),
            *expected,
            "{left} and {right}"
        );
    }
}

#[test]
fn an_undefined_member_differs_from_an_absent_one() {
    let left = Value::Object([("a".to_string(), Value::Undefined)].into_iter().collect());
    let right = Value::Object(Default::default());
    assert!(!equal(&left, &right));
}
