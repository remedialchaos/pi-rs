use pi_rs_codemode::{
    identifier::to_codemode_identifier, schema::*, source::parse_codemode_source,
};
use serde_json::{Value, json};
#[test]
fn declaration_signatures_normalize_names_and_render_both_schemas() -> Result<(), String> {
    assert_eq!(
        render_tool_signature(
            json!({"name":"hidden-dynamic-tool","inputSchema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false},"outputSchema":{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}}),
            None
        )?,
        "hidden_dynamic_tool(args: { city: string; }): Promise<{ ok: boolean; }>;"
    );
    assert_eq!(
        render_tool_signature(json!({"name":"free"}), None)?,
        "free(args: unknown): Promise<unknown>;"
    );
    Ok(())
}
#[test]
fn declaration_samples_include_description_and_fenced_signature() -> Result<(), String> {
    assert_eq!(
        render_tool_sample(
            json!({"name":"foo","description":"bar","inputSchema":{"type":"string"}}),
            None
        )?,
        "bar\n\ncodemode tool declaration:\n```ts\ndeclare const tools: { foo(args: string): Promise<unknown>; };\n```"
    );
    Ok(())
}
#[test]
fn declaration_namespaces_preserve_order_and_explicit_signatures() -> Result<(), String> {
    assert_eq!(
        render_declarations(
            json!({"globals":[{"name":"models.list","description":"List models.","signature":"(type: string): Promise<string[]>"},{"name":"models.get","inputSchema":{"type":"string"}},{"name":"plain","signature":"(): void"}]})
        )?,
        "declare function plain(): void;\n\ndeclare const models: {\n  /** List models. */\n  list(type: string): Promise<string[]>;\n  get(args: string): Promise<unknown>;\n};"
    );
    Ok(())
}
#[test]
fn declaration_descriptions_escape_comment_terminators_and_multiline_text() -> Result<(), String> {
    assert_eq!(
        render_declarations(json!({"tools":[{"name":"x","description":" a */ b\nsecond "}]}))?,
        "declare const tools: {\n  /**\n   * a *\\/ b\n   * second\n   */\n  x(args: unknown): Promise<unknown>;\n};"
    );
    Ok(())
}
#[test]
fn declaration_input_budget_does_not_limit_output_or_explicit_globals() -> Result<(), String> {
    assert_eq!(
        render_tool_signature(
            json!({"name":"x","inputSchema":{"type":"string"},"outputSchema":{"type":"string"}}),
            Some(json!({"inputMaxChars":0}))
        )?,
        "x(args: unknown): Promise<string>;"
    );
    Ok(())
}
#[test]
fn identifier_uses_code_points_and_ascii_identifier_rules() -> Result<(), String> {
    for (input, expected) in [
        ("", "_"),
        ("1name", "_name"),
        ("a-😀.b", "a___b"),
        ("$x_y2", "$x_y2"),
        ("ünïcödé", "_n_c_d_"),
    ] {
        assert_eq!(to_codemode_identifier(input.into())?, expected);
    }
    Ok(())
}
#[test]
fn source_preserves_plain_code_and_directive_line_numbers() -> Result<(), String> {
    for input in ["text('hi')", "// just a comment\nreturn 1"] {
        assert_eq!(
            parse_codemode_source(input.into())?,
            json!({"code":input,"options":{}})
        );
    }
    assert_eq!(
        parse_codemode_source(
            "  // @options:{\"max_output_tokens\":0,\"timeout_ms\":1500}\r\ntext(1)".into()
        )?,
        json!({"code":"\ntext(1)","options":{"maxOutputTokens":0,"timeoutMs":1500}})
    );
    Ok(())
}
#[test]
fn source_rejects_misplaced_directives_and_invalid_option_values() {
    for input in [
        "\n// @options: {}\ntext(1)",
        "// @options: []\ntext(1)",
        "// @options: {\"timeout_ms\":0}\ntext(1)",
        "// @options: {\"timeout_ms\":1}",
        "// @options: {\"max_output_tokens\":1.5}\ntext(1)",
        "// @options: {\"unknown\":1}\ntext(1)",
        " ",
    ] {
        assert!(parse_codemode_source(input.into()).is_err(), "{input}");
    }
}
#[test]
fn recorded_leaf_cases_replay_through_public_rust_functions() -> Result<(), String> {
    let units = [
        ("identifier", include_str!("fixtures/identifier.jsonl")),
        ("source", include_str!("fixtures/source.jsonl")),
        ("declarations", include_str!("fixtures/declarations.jsonl")),
    ];
    let mut count = 0;
    for (unit, cases) in units {
        for (line, case) in cases.lines().enumerate() {
            let case: Value = serde_json::from_str(case).map_err(|e| e.to_string())?;
            let args = case["args"].as_array().ok_or("missing args")?;
            let arg = |i| args.get(i).cloned().unwrap_or(json!({"$undefined":true}));
            let undefined = |v: &Value| v.get("$undefined") == Some(&json!(true));
            let result = match case["fn"].as_str() {
                Some("toCodemodeIdentifier") => to_codemode_identifier(
                    arg(0).as_str().ok_or("expected identifier string")?.into(),
                )
                .map(Value::String),
                Some("parseCodemodeSource") => {
                    parse_codemode_source(arg(0).as_str().ok_or("expected source string")?.into())
                }
                Some("schemaToType") => schema_to_type(arg(0), Some(arg(1))).map(Value::String),
                Some("renderToolOutputType") => {
                    render_tool_output_type((!undefined(&arg(0))).then(|| arg(0)))
                        .map(Value::String)
                }
                Some("resolveSchemaRef") => resolve_schema_ref(
                    arg(0).as_str().ok_or("expected reference string")?.into(),
                    arg(1),
                )
                .map(|v| v.unwrap_or(json!({"$undefined":true}))),
                _ => return Err("unrecognized recorded function".into()),
            };
            match (result, case.get("ok"), case.get("error")) {
                (Ok(actual), Some(expected), _) => {
                    assert_eq!(&actual, expected, "{unit} case {}", line + 1)
                }
                (Err(actual), _, Some(error)) => assert_eq!(
                    actual,
                    error
                        .as_str()
                        .or_else(|| error.get("message").and_then(Value::as_str))
                        .unwrap_or(""),
                    "{unit} case {}",
                    line + 1
                ),
                (_, _, _) => {
                    return Err(format!(
                        "unhandled recorded case {unit}:{} {case}",
                        line + 1
                    ));
                }
            }
            count += 1;
        }
    }
    assert_eq!(count, 1174);
    Ok(())
}
