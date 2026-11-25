use pi_rs_codemode::schema::*;
use serde_json::{Value, json};
fn render(schema: Value, expected: &str) -> Result<(), String> {
    assert_eq!(schema_to_type(schema, None)?, expected);
    Ok(())
}
#[test]
fn schema_primitives_literals_and_union_precedence() -> Result<(), String> {
    for (schema, expected) in [
        (json!({"type":"string"}), "string"),
        (json!({"type":"integer"}), "number"),
        (json!({"type":["string","null"]}), "string | null"),
        (json!({"const":"a"}), "\"a\""),
        (json!({"enum":["a",1,null]}), "\"a\" | 1 | null"),
        (
            json!({"anyOf":[{"type":"string"},{"type":"number"}]}),
            "string | number",
        ),
        (json!({"anyOf":[{"type":"string"},{}]}), "unknown"),
        (
            json!({"allOf":[{"anyOf":[{"type":"string"},{"type":"number"}]},{"const":1}]}),
            "(string | number) & 1",
        ),
        (json!({"$ref":"#/defs/x"}), "unknown"),
        (json!(true), "unknown"),
        (json!(false), "never"),
    ] {
        render(schema, expected)?;
    }
    Ok(())
}
#[test]
fn schema_sorted_properties_and_additional_property_types() -> Result<(), String> {
    for (schema, expected) in [
        (
            json!({"type":"object","properties":{"city":{"type":"string"},"max-lines":{"type":"number"}},"required":["city"],"additionalProperties":false}),
            "{ city: string; \"max-lines\"?: number; }",
        ),
        (
            json!({"type":"object","additionalProperties":{"type":"number"}}),
            "{ [key: string]: number; }",
        ),
        (json!({"type":"object"}), "{ [key: string]: unknown; }"),
        (
            json!({"type":"object","properties":{},"additionalProperties":false}),
            "{}",
        ),
    ] {
        render(schema, expected)?;
    }
    Ok(())
}
#[test]
fn schema_nested_property_comments() -> Result<(), String> {
    render(
        json!({"type":"object","properties":{"weather":{"type":"array","description":"look up weather for a given list of locations","items":{"type":"object","properties":{"location":{"type":"string"}},"required":["location"]}}},"required":["weather"]}),
        "{\n  // look up weather for a given list of locations\n  weather: Array<{ location: string; }>;\n}",
    )?;
    render(
        json!({"type":"object","properties":{"outer":{"type":"object","description":"Outer","properties":{"inner":{"type":"string","description":"Inner"}}}}}),
        "{\n  // Outer\n  outer?: {\n    // Inner\n    inner?: string;\n  };\n}",
    )
}
#[test]
fn schema_constraints_not_expressible_in_types() -> Result<(), String> {
    render(
        json!({"type":"object","properties":{"timeoutMs":{"type":"integer","minimum":0,"maximum":30000,"default":0,"description":"Wait in milliseconds."},"id":{"type":"string","pattern":"^[a-z]+$","maxLength":8},"tags":{"type":"array","items":{"type":"string"},"maxItems":5,"uniqueItems":true,"deprecated":true},"name":{"type":"string"}},"required":["id"]}),
        "{\n  // @maxLength 8 @pattern ^[a-z]+$\n  id: string;\n  name?: string;\n  // @maxItems 5 @uniqueItems @deprecated\n  tags?: Array<string>;\n  // Wait in milliseconds.\n  // @integer @minimum 0 @maximum 30000 @default 0\n  timeoutMs?: number;\n}",
    )
}
#[test]
fn schema_optional_null_constraints_intersect_parent_bounds() -> Result<(), String> {
    render(
        json!({"type":"object","properties":{"limit":{"anyOf":[{"type":"integer","minimum":1,"maximum":100},{"type":"null"}],"default":null,"title":"Limit"}}}),
        "{\n  // @integer @minimum 1 @maximum 100\n  limit?: number | null;\n}",
    )?;
    render(
        json!({"type":"object","properties":{"n":{"anyOf":[{"type":"integer","minimum":10,"maximum":50,"format":"int32"},{"type":"null"}],"minimum":0,"maximum":20,"format":"port"}}}),
        "{\n  // @integer @minimum 10 @maximum 20 @format port @format int32\n  n?: number | null;\n}",
    )
}
#[test]
fn schema_draft04_boolean_exclusive_bounds() -> Result<(), String> {
    render(
        json!({"type":"object","properties":{"n":{"type":"integer","minimum":0,"exclusiveMinimum":true,"maximum":10,"exclusiveMaximum":false},"m":{"anyOf":[{"type":"number","maximum":5,"exclusiveMaximum":true},{"type":"null"}],"maximum":8}}}),
        "{\n  // @maximum 8 @exclusiveMaximum 5\n  m?: number | null;\n  // @integer @exclusiveMinimum 0 @maximum 10\n  n?: number;\n}",
    )
}
#[test]
fn schema_long_tags_and_line_terminators_are_safe() -> Result<(), String> {
    render(
        json!({"type":"object","properties":{"config":{"type":"object","default":{"text":"x".repeat(100)}}}}),
        "{ config?: { [key: string]: unknown; }; }",
    )?;
    render(
        json!({"type":"object","properties":{"sep":{"type":"string","pattern":"^[\r\n\u{2028}\u{2029}]$","default":"\u{2028}","description":"a\u{2028}b"}}}),
        "{\n  // a\n  // b\n  // @pattern ^[\\u000d\\u000a\\u2028\\u2029]$ @default \"\\u2028\"\n  sep?: string;\n}",
    )
}
#[test]
fn schema_budget_drops_tags_before_types() -> Result<(), String> {
    let schema = json!({"type":"object","properties":{"n":{"type":"integer","minimum":1}}});
    render(
        schema.clone(),
        "{\n  // @integer @minimum 1\n  n?: number;\n}",
    )?;
    assert_eq!(
        schema_to_type(schema.clone(), Some(json!({"maxChars":20})))?,
        "{ n?: number; }"
    );
    assert_eq!(
        schema_to_type(schema, Some(json!({"maxChars":10})))?,
        "unknown"
    );
    Ok(())
}
#[test]
fn schema_recursive_references_stop_and_remote_refs_are_unknown() -> Result<(), String> {
    render(
        json!({"type":"object","properties":{"item":{"$ref":"#/$defs/Item"},"legacy":{"$ref":"#/definitions/Legacy"},"remote":{"$ref":"https://example.com/schema.json"}},"required":["item"],"$defs":{"Item":{"type":"object","properties":{"id":{"type":"string"},"parent":{"$ref":"#/$defs/Item"}},"required":["id"]}},"definitions":{"Legacy":{"enum":["a","b"]}}}),
        "{ item: { id: string; parent?: unknown; }; legacy?: \"a\" | \"b\"; remote?: unknown; }",
    )
}
#[test]
fn schema_arrays_and_tuples() -> Result<(), String> {
    render(
        json!({"type":"array","items":{"type":"string"}}),
        "Array<string>",
    )?;
    render(
        json!({"type":"array","prefixItems":[{"type":"string"},{"type":"number"}]}),
        "[string, number]",
    )?;
    render(json!({"type":"array"}), "unknown[]")
}
#[test]
fn schema_large_types_respect_budget() -> Result<(), String> {
    let properties: serde_json::Map<String, Value> = (0..50)
        .map(|i| (format!("field{i}"), json!({"type":"string"})))
        .collect();
    let schema = json!({"type":"object","properties":properties});
    assert_eq!(
        schema_to_type(schema.clone(), Some(json!({"maxChars":100})))?,
        "unknown"
    );
    assert!(schema_to_type(schema, None)?.contains("field49?: string;"));
    Ok(())
}
#[test]
fn schema_pointer_escapes_and_invalid_references() -> Result<(), String> {
    let root = json!({"$defs":{"item":{"type":"string"},"a/b":{"type":"string"},"c~d":{"type":"string"},"e f":{"type":"string"}}});
    assert_eq!(
        resolve_schema_ref("#".into(), root.clone())?,
        Some(root.clone())
    );
    for pointer in [
        "#/$defs/item",
        "#/$defs/a~1b",
        "#/$defs/c~0d",
        "#/$defs/e%20f",
    ] {
        assert_eq!(
            resolve_schema_ref(pointer.into(), root.clone())?,
            Some(json!({"type":"string"}))
        );
    }
    for pointer in [
        "other.json#/$defs/item",
        "#/$defs/missing",
        "#/$defs/%E0%A4%A",
    ] {
        assert_eq!(resolve_schema_ref(pointer.into(), root.clone())?, None);
    }
    Ok(())
}
#[test]
fn mcp_call_tool_result_output_is_an_ordinary_type() -> Result<(), String> {
    assert_eq!(
        render_tool_signature(
            json!({"name":"plain","inputSchema":{"type":"object","properties":{}},"outputSchema":{"type":"object","properties":{"content":{"type":"array","items":{"type":"object"}},"isError":{"type":"boolean"},"_meta":{"type":"object"}},"required":["content"]}}),
            None
        )?,
        "plain(args: { [key: string]: unknown; }): Promise<{ _meta?: { [key: string]: unknown; }; content: Array<{ [key: string]: unknown; }>; isError?: boolean; }>;"
    );
    Ok(())
}
#[test]
fn complete_tools_and_globals_declarations_match_upstream() -> Result<(), String> {
    assert_eq!(
        render_declarations(
            json!({"tools":[{"name":"read","description":"Read a file.\nSecond line.","inputSchema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]},"outputSchema":{"type":"string"}},{"name":"remote-api"}],"globals":[{"name":"attach","description":"Attach it.","inputSchema":{"type":"string"}}]})
        )?,
        "declare const tools: {\n  /**\n   * Read a file.\n   * Second line.\n   */\n  read(args: { path: string; }): Promise<string>;\n  remote_api(args: unknown): Promise<unknown>;\n};\n\n/** Attach it. */\ndeclare function attach(args: string): Promise<unknown>;"
    );
    Ok(())
}
