//! `packages/mcp/test/input-schema.test.ts`.

use pi_rs_mcp::normalize_mcp_input_schema;
use serde_json::json;

#[test]
fn adds_type_object_and_empty_properties_when_missing() {
    assert_eq!(
        normalize_mcp_input_schema(&json!({})),
        Ok(json!({ "type": "object", "properties": {} }))
    );
    assert_eq!(
        normalize_mcp_input_schema(&json!({ "type": "object" })),
        Ok(json!({ "type": "object", "properties": {} }))
    );
}

#[test]
fn keeps_the_schemas_own_keywords() {
    let schema = json!({
        "type": "object",
        "properties": { "query": { "type": "string" } },
        "required": ["query"],
        "additionalProperties": false,
    });
    assert_eq!(normalize_mcp_input_schema(&schema), Ok(schema));
    assert_eq!(
        normalize_mcp_input_schema(&json!({ "properties": { "a": {} }, "$defs": { "x": {} } })),
        Ok(json!({
            "properties": { "a": {} },
            "$defs": { "x": {} },
            "type": "object",
        }))
    );
}
