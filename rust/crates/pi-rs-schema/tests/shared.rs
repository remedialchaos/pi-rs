// Values shared across compiled schemas hold nothing alive: a cached pattern goes once its schemas are collected.

use pi_rs_schema::{Value, compile, regex};
use serde_json::json;

#[test]
fn shares_the_module_level_check_of_a_bare_type() {
    let first =
        compile(&json!({ "type": "string", "description": "a" })).expect("the schema compiles");
    let second = compile(&json!({ "type": "string", "title": "b" })).expect("the schema compiles");
    assert_eq!(first.root_check(), second.root_check());
}

#[test]
fn shares_a_patterns_regular_expression_and_drops_it_with_the_last_schema_that_uses_it() {
    let before = regex::count();
    let validators: Vec<_> = (0..50)
        .map(|index| compile(&json!({ "type": "string", "pattern": format!("^unique-{index}$") })))
        .collect::<Result<Vec<_>, _>>()
        .expect("the schemas compile");
    let again = compile(&json!({ "type": "string", "pattern": "^unique-0$" }))
        .expect("the schema compiles");
    assert_eq!(regex::count(), before + 50);
    assert!(again.check(&Value::String("unique-0".to_string())));
    drop(validators);
    // `again` still uses ^unique-0$; the other 49 patterns are gone.
    assert_eq!(regex::count(), before + 1);
}
