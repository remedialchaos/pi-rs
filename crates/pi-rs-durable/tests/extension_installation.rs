use pi_rs_durable::{
    codemode::{CodemodeToolOptions, create_codemode_harness_extension},
    harness::{create_registry, resolve_agent},
    tool_search::create_tool_search_extension,
    tools::create_coding_tools_extension,
};
use serde_json::json;

#[test]
fn installed_extensions_separate_model_tools_from_nested_calls_and_prepare_edits()
-> Result<(), Box<dyn std::error::Error>> {
    let registry = create_registry();
    registry.install(create_coding_tools_extension(None))?;
    registry.install(create_tool_search_extension())?;
    registry.install(create_codemode_harness_extension(
        CodemodeToolOptions::default(),
    ))?;
    let agent = resolve_agent(
        json!({"extensions":["coding-tools","tool-search","codemode"]}),
        &registry.snapshot(),
        None,
        |_| {},
    );
    assert_eq!(
        agent
            .tools
            .iter()
            .map(|t| t.declaration.name.as_str())
            .collect::<Vec<_>>(),
        ["read", "write", "edit", "bash", "tool_search", "codemode"]
    );
    let nested: Vec<_> = agent
        .callable
        .iter()
        .map(|t| t.declaration.name.as_str())
        .collect();
    assert_eq!(nested.len(), 6);
    assert!(!nested.contains(&"tool_search") && !nested.contains(&"codemode"));
    let edit = agent
        .tools
        .iter()
        .find(|t| t.declaration.name == "edit")
        .ok_or("edit missing")?;
    let prepared = edit.prepare_arguments.as_ref().ok_or("prepare missing")?(
        json!({"path":"p","oldText":"a","newText":"b"}),
    )?;
    assert_eq!(
        prepared,
        json!({"path":"p","edits":[{"oldText":"a","newText":"b"}]})
    );
    Ok(())
}
