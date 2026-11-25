//! `powershell` built-in plugin: offers Durable's deferred `powershell` tool on Windows
//! environments when `bash` is enabled (`apps/p2/plugins/powershell/worker.ts`).

use std::sync::Arc;

use pi_rs_ai::Tool;
use pi_rs_chord::{
    Result,
    contracts::Contract,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
};
use pi_rs_durable::{
    harness::{Extension, ToolRegistration, define_extension},
    session::Error as DurableError,
    tools::{PowerShellToolInput, PowerShellToolOptions, TailShellOutput, create_powershell_tool},
};
use serde_json::{Value, json};

use crate::api::v1::{FacetKind, PluginRegistry, worker::worker_of};

pub const POWERSHELL_EXTENSION: &str = "p2-powershell";
pub const POWERSHELL_TOOL: &str = "powershell";

/// Whether `agent_state` enables `tool_name`.
pub fn enables_tool(agent_state: Option<&Value>, tool_name: &str) -> bool {
    let Some(tools) = agent_state.and_then(|s| s.get("tools")) else {
        return true;
    };
    if let Some(arr) = tools.as_array() {
        return arr.iter().any(|v| v.as_str() == Some(tool_name));
    }
    if let Some(removed) = tools.get("remove").and_then(Value::as_array) {
        return !removed.iter().any(|v| v.as_str() == Some(tool_name));
    }
    true
}

/// Whether `agent_state` currently offers the deferred `powershell` tool to the model.
pub fn offers_powershell(agent_state: Option<&Value>) -> bool {
    if !enables_tool(agent_state, POWERSHELL_TOOL) {
        return false;
    }
    let Some(model_tools) = agent_state.and_then(|s| s.get("modelTools")) else {
        return false;
    };
    if let Some(arr) = model_tools.as_array() {
        return arr.iter().any(|v| v.as_str() == Some(POWERSHELL_TOOL));
    }
    model_tools
        .get("add")
        .and_then(Value::as_array)
        .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some(POWERSHELL_TOOL)))
}

/// Enables or disables `powershell` in `agent_state` while preserving all other tool choices.
pub fn set_powershell(agent_state: &mut Value, on: bool) {
    let Some(obj) = agent_state.as_object_mut() else {
        return;
    };
    let without = |arr: &[Value]| -> Vec<Value> {
        arr.iter()
            .filter(|v| v.as_str() != Some(POWERSHELL_TOOL))
            .cloned()
            .collect()
    };

    if let Some(tools) = obj.get("tools").cloned() {
        if let Some(arr) = tools.as_array() {
            let mut next = without(arr);
            if on {
                next.push(Value::String(POWERSHELL_TOOL.into()));
            }
            obj.insert("tools".into(), Value::Array(next));
        } else if on
            && let Some(removed) = tools.get("remove").and_then(Value::as_array)
            && removed.iter().any(|v| v.as_str() == Some(POWERSHELL_TOOL))
        {
            obj.insert("tools".into(), json!({ "remove": without(removed) }));
        }
    }

    if let Some(model_tools) = obj.get("modelTools").cloned()
        && let Some(arr) = model_tools.as_array()
    {
        let mut next = without(arr);
        if on {
            next.push(Value::String(POWERSHELL_TOOL.into()));
        }
        obj.insert("modelTools".into(), Value::Array(next));
        return;
    }

    let mut add = obj
        .get("modelTools")
        .and_then(|mt| mt.get("add"))
        .and_then(Value::as_array)
        .map(|arr| without(arr))
        .unwrap_or_default();
    if on {
        add.push(Value::String(POWERSHELL_TOOL.into()));
    }
    let remove = obj
        .get("modelTools")
        .and_then(|mt| mt.get("remove"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut next_mt = serde_json::Map::new();
    if !remove.is_empty() {
        next_mt.insert("remove".into(), Value::Array(remove));
    }
    if !add.is_empty() {
        next_mt.insert("add".into(), Value::Array(add));
    }
    obj.insert("modelTools".into(), Value::Object(next_mt));
}

/// Builds the Durable `p2-powershell` extension wrapping Durable's deferred `powershell` tool.
pub fn create_powershell_extension() -> Extension {
    let tool_impl = Arc::new(create_powershell_tool(PowerShellToolOptions::default()));
    let spec = tool_impl.spec();
    let registration = ToolRegistration {
        declaration: Tool {
            name: spec.name.to_owned(),
            description: spec.description.clone(),
            parameters: spec.parameters.clone(),
            constrained_sampling: None,
        },
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: spec.structured_output_schema.clone(),
        output_limits: spec.output_limits,
        replay_safe: false,
        callers: None,
        deferred: true,
        execute: Arc::new(move |args, invocation, context| {
            let tool_impl = tool_impl.clone();
            Box::pin(async move {
                let parsed: PowerShellToolInput = serde_json::from_value(args)?;
                let env = invocation.env(context).await?;
                let mut shell_out = TailShellOutput::new(None);
                let result = tool_impl
                    .execute(parsed, env.as_deref(), &mut shell_out, context)
                    .await
                    .map_err(|e| DurableError::invalid(e.to_string()))?;
                Ok(pi_rs_durable::tools::ToolResult {
                    output: result.output,
                    structured_output: result.structured_output,
                    is_error: result.is_error,
                    details: result.details.map(serde_json::to_value).transpose()?,
                    diagnostics: result.diagnostics,
                    control: result.control,
                    usage: result.usage,
                }
                .into())
            })
        }),
    };

    define_extension(Extension {
        name: POWERSHELL_EXTENSION.into(),
        tools: vec![registration],
        ..Default::default()
    })
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let mut member_defs = indexmap::IndexMap::new();
    member_defs.insert(
        "offered".into(),
        pi_rs_chord::contracts::Member::State(JsonValue::from_serde(json!({ "type": "object" }))),
    );
    let contract = Arc::new(Contract::local(
        "p2.powershell",
        1,
        Vec::new(),
        member_defs,
    )?);
    registry.register(
        "powershell",
        FacetKind::Worker,
        define_facet(move |host| {
            host.serve(
                contract.clone(),
                move |_, _, object| {
                    let offered = object.writable(JsonValue::from_serde(json!({})))?;
                    if let Ok(worker) = worker_of(&object.host()) {
                        let extension = create_powershell_extension();
                        let ext_name = extension.name.clone();
                        let registries = worker.registries.clone();
                        registries.extensions.install(extension)?;
                        let _cleanup = object.open(
                            |_| Box::pin(async { Ok(()) }),
                            move |_, _| {
                                let registries = registries.clone();
                                let ext_name = ext_name.clone();
                                Box::pin(async move {
                                    let _ = registries.extensions.uninstall(&ext_name);
                                    Ok(())
                                })
                            },
                        );
                    }
                    let mut members = Members::new();
                    members.state("offered", offered)?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )
}
