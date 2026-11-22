//! `packages/mcp/test/result-schema.test.ts`.

use pi_rs_mcp::create_mcp_result_schema;
use serde_json::json;

#[test]
fn declares_the_call_tool_result_with_any_structured_content_without_an_output_schema() {
    let schema = create_mcp_result_schema(None);
    let object = schema.as_object().cloned().unwrap_or_default();
    let properties = object
        .get("properties")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    assert_eq!(
        properties.keys().collect::<Vec<_>>(),
        vec!["content", "structuredContent", "isError"]
    );
    assert_eq!(properties.get("structuredContent"), Some(&json!({})));
    assert_eq!(object.get("required"), Some(&json!(["content"])));
    assert_eq!(object.get("additionalProperties"), Some(&json!(true)));
}

#[test]
fn moves_the_output_schemas_local_references_below_structured_content() {
    let hit = json!({ "type": "object", "properties": { "id": { "type": "string" } } });
    let schema = create_mcp_result_schema(Some(&json!({
        "type": "object",
        "properties": {
            "hits": { "type": "array", "items": { "$ref": "#/$defs/Hit" } },
            "children": { "type": "array", "items": { "$ref": "#" } },
            // A property named like a keyword: its value is a schema.
            "default": { "$ref": "#/definitions/Hit" },
            "remote": { "$ref": "https://example.com/schema.json" },
        },
        "$defs": { "Hit": { "default": { "$ref": "#/stays" }, "type": "object", "properties": { "id": { "type": "string" } } } },
        "definitions": { "Hit": hit },
    })));
    let moved = schema
        .pointer("/properties/structuredContent")
        .cloned()
        .unwrap_or_default();
    let at = "#/properties/structuredContent";
    assert_eq!(
        moved.pointer("/properties/hits"),
        Some(&json!({ "type": "array", "items": { "$ref": format!("{at}/$defs/Hit") } }))
    );
    assert_eq!(
        moved.pointer("/properties/children"),
        Some(&json!({ "type": "array", "items": { "$ref": at } }))
    );
    assert_eq!(
        moved.pointer("/properties/default"),
        Some(&json!({ "$ref": format!("{at}/definitions/Hit") }))
    );
    assert_eq!(
        moved.pointer("/properties/remote"),
        Some(&json!({ "$ref": "https://example.com/schema.json" }))
    );
    assert_eq!(
        moved.pointer("/$defs/Hit/default"),
        Some(&json!({ "$ref": "#/stays" }))
    );
}
