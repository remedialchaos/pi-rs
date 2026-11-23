use std::{collections::BTreeMap, error::Error, sync::Arc};

use indexmap::IndexMap;
use pi_rs_ai::{
    ThinkingLevel, Tool,
    faux::{FauxModelDefinition, FauxOptions, faux_provider},
};
use pi_rs_chord::{context::Context, host::Caller, json::JsonValue};
use pi_rs_durable::harness::{Agent, TaskGraph, TaskGraphNode, ToolRegistration};
use pi_rs_p2::{
    api::v1::{FacetKind, GrantMode, OwnerAccess, PluginRegistry, Principal},
    hosts::server::{ServerOptions, start_server},
    plugins::{
        register_builtins,
        subagents::{
            children::child_rows_history,
            contract::{
                Child, ChildFailure, by_recent_activity, child_store_contract,
                conversation_location, model_ref_of, remote_environments, short_model_name,
                strip_children_map_v3, subagent_control_contract, subagent_sender_text,
                subagents_contract,
            },
            contract_worker::{
                AgentChoice, AgentKind, Subagent, ToolScope, choosable_tools, resolve_choice,
                resolve_tools, still_given,
            },
            model_list::{ListedModel, close_models, list_models},
            worker::{FailureSource, Seen, failure_source, working_children},
        },
    },
    runtime::location::Location,
};
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn dummy_tool(name: &str, callers: Option<Vec<&str>>, deferred: bool) -> ToolRegistration {
    ToolRegistration {
        declaration: Tool {
            name: name.into(),
            description: format!("{name} tool"),
            parameters: json!({ "type": "object", "properties": {} }),
            constrained_sampling: None,
        },
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: None,
        output_limits: None,
        replay_safe: true,
        callers: callers.map(|c| c.into_iter().map(str::to_owned).collect()),
        deferred,
        execute: Arc::new(|_, _, _| Box::pin(async { unreachable!() })),
    }
}

#[test]
fn subagents_contracts_and_helpers() -> TestResult {
    let mut registry = PluginRegistry::new();
    register_builtins(&mut registry)?;
    let selected = registry.select(None)?;
    assert!(selected.contains(&"subagents".to_owned()));
    assert_eq!(registry.facets(FacetKind::Server, &selected).len(), 1);
    assert_eq!(registry.facets(FacetKind::Worker, &selected).len(), 1);

    assert_eq!(child_store_contract()?.name(), "p2.child-store");
    assert_eq!(subagents_contract()?.name(), "p2.subagents");
    assert_eq!(subagent_control_contract()?.name(), "p2.subagent-control");
    assert_eq!(child_rows_history()?.definition.version, 2);

    assert_eq!(
        model_ref_of("anthropic/claude-sonnet-4"),
        Some(("anthropic", "claude-sonnet-4"))
    );
    assert_eq!(
        model_ref_of("openrouter/org/model"),
        Some(("openrouter", "org/model"))
    );
    assert_eq!(model_ref_of("noslash"), None);
    assert_eq!(model_ref_of("/leading"), None);
    assert_eq!(short_model_name("openrouter/org/model"), "model");
    assert_eq!(
        subagent_sender_text("a\"b<c>&"),
        "<sender kind=\"subagent\" name=\"a&quot;b&lt;c&gt;&amp;\"/>"
    );

    let c1 = Child {
        name: "alpha".into(),
        kind: None,
        call_id: None,
        archived: None,
        conversation_id: 2,
        model: "p/m".into(),
        thinking_level: "off".into(),
        tools: vec![],
        location: Some(Location {
            cwd: "/wt".into(),
            env: Some("box-a".into()),
        }),
        last_entry_id: 10,
        updated_at: Some(1234),
        failure: Some(ChildFailure {
            reason: "aborted".into(),
            detail: Some("secret".into()),
        }),
    };
    let c2 = Child {
        name: "beta".into(),
        conversation_id: 3,
        last_entry_id: 20,
        location: None,
        ..c1.clone()
    };
    assert!(by_recent_activity(&c2, &c1).is_lt());
    assert_eq!(
        conversation_location(Some("/sess"), Some("main"), Some(&c1)).map(|l| l.cwd),
        Some("/wt".into())
    );
    assert_eq!(
        conversation_location(Some("/sess"), Some("main"), Some(&c2)),
        Some(Location {
            cwd: "/sess".into(),
            env: Some("main".into()),
        })
    );
    assert_eq!(
        remote_environments(Some("main"), &[c1.clone(), c2]),
        vec!["main".to_owned(), "box-a".to_owned()]
    );

    let stripped = strip_children_map_v3(json!({ "s1": [c1] }));
    assert!(stripped["s1"][0].get("updatedAt").is_none());
    assert!(stripped["s1"][0].get("failure").is_none());
    Ok(())
}

#[test]
fn subagents_model_list_and_close_matches() {
    assert_eq!(
        list_models(&[], None),
        "No models with configured credentials."
    );
    assert_eq!(
        list_models(&[], Some("sonnet")),
        "No available model matches sonnet."
    );

    let small = vec![
        ListedModel {
            provider: "anthropic".into(),
            id: "claude-fable-5-2".into(),
            name: "Claude Fable 5.2".into(),
        },
        ListedModel {
            provider: "openai".into(),
            id: "fable-5-2".into(),
            name: "Fable 5.2".into(),
        },
    ];
    assert_eq!(
        list_models(&small, Some("5 2")),
        "anthropic/claude-fable-5-2 (Claude Fable 5.2)\nopenai/fable-5-2 (Fable 5.2)"
    );
    assert_eq!(
        close_models(&small, "anthropic/fable-5.2"),
        vec!["anthropic/claude-fable-5-2"]
    );
    assert_eq!(
        close_models(&small, "fable-5.2"),
        vec!["anthropic/claude-fable-5-2", "openai/fable-5-2"]
    );

    let many: Vec<ListedModel> = (0..105)
        .map(|i| ListedModel {
            provider: if i < 60 { "anthropic" } else { "openai" }.into(),
            id: format!("model-{i}"),
            name: format!("Model {i}"),
        })
        .collect();
    let grouped = list_models(&many, None);
    assert!(grouped.contains("105 models, too many to list. By provider:"));
    assert!(grouped.contains("anthropic: 60"));
    assert!(grouped.contains("openai: 45"));
}

#[tokio::test]
async fn subagents_tool_and_choice_resolution() -> TestResult {
    let caller = Agent {
        state: json!({ "model": { "provider": "anthropic", "modelId": "claude-sonnet-4" } }),
        extensions: vec![],
        tools: vec![
            dummy_tool("read", Some(vec!["model"]), false),
            dummy_tool("codemode", Some(vec!["model"]), false),
            dummy_tool("subagent", Some(vec!["model"]), false),
        ],
        deferred: vec![
            dummy_tool("team", Some(vec!["model"]), true),
            dummy_tool("web_search", Some(vec!["model"]), true),
        ],
        callable: vec![
            dummy_tool("bash", Some(vec!["script"]), false),
            dummy_tool("main_script", Some(vec!["script"]), false),
        ],
        sections: vec![],
    };

    let mut access = IndexMap::new();
    access.insert("subagent".into(), ToolScope::Main);
    access.insert("team".into(), ToolScope::Subagent);
    access.insert("main_script".into(), ToolScope::Main);

    let choosable: Vec<String> = choosable_tools(&caller, &access)
        .into_iter()
        .map(|t| t.declaration.name)
        .collect();
    assert_eq!(choosable, vec!["read", "codemode", "web_search"]);

    let resolved = resolve_tools(
        &caller,
        &access,
        Some(&["Read".into(), "codemode".into(), "web_search".into()]),
        &["web_search".into()],
        AgentKind::Subagent,
    )
    .map_err(|e| format!("unexpected error: {e}"))?;
    assert_eq!(resolved.chosen, vec!["read", "codemode", "web_search"]);
    let tool_names: Vec<&str> = resolved
        .tools
        .iter()
        .map(|t| t.declaration.name.as_str())
        .collect();
    assert_eq!(
        tool_names,
        vec!["read", "codemode", "web_search", "bash", "team"]
    );
    let loaded_names: Vec<&str> = resolved
        .loaded
        .iter()
        .map(|t| t.declaration.name.as_str())
        .collect();
    assert_eq!(loaded_names, vec!["team", "web_search"]);

    let current = Subagent {
        conversation_id: 2,
        model: "anthropic/claude-sonnet-4".into(),
        thinking_level: "high".into(),
        tools: vec!["read".into(), "removed_tool".into()],
        location: None,
        unplaced_cwd: None,
        parent: None,
        reported: vec![],
        archived: None,
    };
    assert_eq!(
        still_given(&caller, &access, Some(&current)),
        Some(vec!["read".into()])
    );

    let models = pi_rs_ai::registry::Models::default();
    let faux = faux_provider(FauxOptions {
        provider: Some("anthropic".into()),
        models: vec![FauxModelDefinition {
            id: "claude-sonnet-4".into(),
            name: Some("Claude Sonnet 4".into()),
            reasoning: false,
            ..Default::default()
        }],
        ..Default::default()
    });
    models.set_provider(faux.provider.clone());
    let ctx = Context::background();

    let err = resolve_choice(
        &models,
        &caller,
        &access,
        &AgentChoice {
            model: Some("anthropic/sonnet 4".into()),
            ..Default::default()
        },
        None,
        AgentKind::Subagent,
        &ctx,
    )
    .await;
    assert!(matches!(err, Err(msg) if msg.contains("Close matches: anthropic/claude-sonnet-4.")));

    let clamped = resolve_choice(
        &models,
        &caller,
        &access,
        &AgentChoice {
            thinking_level: Some("xhigh".into()),
            ..Default::default()
        },
        None,
        AgentKind::Subagent,
        &ctx,
    )
    .await
    .map_err(|e| format!("unexpected error: {e}"))?;
    assert_eq!(clamped.thinking_level, ThinkingLevel::Off);
    Ok(())
}

#[test]
fn subagents_failure_source_and_working_children() {
    let seen = Seen {
        last_entry_id: Some(10),
        updated_at: Some(500),
        failure: Some(ChildFailure {
            reason: "no_model".into(),
            detail: None,
        }),
    };
    assert_eq!(
        failure_source(Some(&seen), Some(10), false),
        FailureSource::Cached(seen.failure.clone())
    );
    assert_eq!(
        failure_source(Some(&seen), Some(11), false),
        FailureSource::Read
    );
    assert_eq!(
        failure_source(Some(&seen), Some(10), true),
        FailureSource::Read
    );
    assert_eq!(failure_source(None, Some(10), false), FailureSource::Read);

    let mut seen_map = BTreeMap::new();
    seen_map.insert(2, seen);
    let mut graph = TaskGraph::default();
    graph.tasks.insert(
        1,
        TaskGraphNode {
            id: 1,
            kind: "t1".into(),
            conversation_id: 2,
            owner: None,
            background: false,
            abort_requested: false,
            state: json!({}),
            blocked: None,
            conversations: vec![2],
        },
    );
    graph.tasks.insert(
        2,
        TaskGraphNode {
            id: 2,
            kind: "t2".into(),
            conversation_id: 3,
            owner: None,
            background: false,
            abort_requested: false,
            state: json!({}),
            blocked: None,
            conversations: vec![3],
        },
    );
    let working = working_children(&graph, &seen_map);
    assert_eq!(working.into_iter().collect::<Vec<_>>(), vec![2]);
}

#[tokio::test]
async fn subagents_server_facet_serves_child_store_and_strips_guest_detail() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut options = ServerOptions::new(dir.path().into());
    options.port = 0;
    register_builtins(&mut options.registry)?;
    let server = start_server(options).await?;
    let ctx = Context::background();
    let root = server.host.root();

    let child_store = root.hold(
        Arc::new(child_store_contract()?),
        JsonValue::Object(IndexMap::new()),
    )?;
    child_store.wait(&ctx).await?;

    // Report from non-running worker is ignored (returns null and writes nothing).
    let child = json!([{
        "name": "worker-a",
        "conversationId": 2,
        "model": "anthropic/claude-sonnet-4",
        "thinkingLevel": "off",
        "tools": ["read"],
        "lastEntryId": 5,
        "updatedAt": 1000,
        "failure": { "reason": "error", "detail": "/secret/path" }
    }]);
    child_store
        .call(
            "report",
            JsonValue::from_serde(json!({
                "session": "sess-1",
                "generation": 1,
                "children": child,
            })),
            &ctx,
        )
        .await?;
    assert_eq!(
        child_store
            .member("children")?
            .value()?
            .to_json()
            .to_serde()?,
        json!({})
    );

    // Owner and guest reader views on p2.subagents:
    let subagents = Arc::new(subagents_contract()?);
    let owner_obj = server.host.hold_as(
        subagents.clone(),
        JsonValue::Object(IndexMap::new()),
        Caller {
            principal: Some(serde_json::to_value(Principal::Owner {
                access: OwnerAccess::Local,
            })?),
            incarnation: uuid::Uuid::new_v4().to_string(),
        },
        &ctx,
    )?;
    owner_obj.wait(&ctx).await?;
    assert_eq!(owner_obj.variant()?, "reader");

    let mut guest_sessions = IndexMap::new();
    guest_sessions.insert("sess-1".into(), GrantMode::View);
    let guest_obj = server.host.hold_as(
        subagents,
        JsonValue::Object(IndexMap::new()),
        Caller {
            principal: Some(serde_json::to_value(Principal::Guest {
                id: "g1".into(),
                name: "Guest".into(),
                email: None,
                home: "sess-1".into(),
                sessions: guest_sessions,
                revision: 1,
            })?),
            incarnation: uuid::Uuid::new_v4().to_string(),
        },
        &ctx,
    )?;
    guest_obj.wait(&ctx).await?;
    assert_eq!(guest_obj.variant()?, "reader");
    assert_eq!(
        guest_obj
            .member("children")?
            .value()?
            .to_json()
            .to_serde()?,
        json!({ "sess-1": [] })
    );

    drop(root);
    let _ = server.close().await;
    Ok(())
}
