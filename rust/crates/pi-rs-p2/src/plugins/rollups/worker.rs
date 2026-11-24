//! The Familiar's `sidebar_rollups` tool (worker, Familiar session only).

use std::sync::Arc;

use indexmap::IndexMap;
use pi_rs_ai::{InputContent, Tool};
use pi_rs_chord::{
    Result,
    context::Context,
    host::{Facet, Host, define_facet},
    json::JsonValue,
};
use pi_rs_durable::{
    harness::{Extension, ToolRegistration, define_extension, define_tool},
    session::Error as DurableError,
    tools::ToolResult,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    api::v1::{
        p2_contracts::lookup,
        worker::{WorkerProcess, durable::ToolAccess, worker_of},
    },
    plugins::familiar::FAMILIAR_INIT,
};

use super::ROLLUPS_TOOL;

const DESCRIPTION: &str = "\
Manage the rollups of the owner's sidebar that are made from code. A rollup is a script over every session that returns one section of the sidebar.
list: the saved rollups with their last result. preview (code): run a script against the sessions now, without saving; check its output before saving.
save (key, name, code; optional window, refresh): save a rollup and run it once; the same key replaces it. refresh (key): run it now. remove (key).

The script is a code mode script body without tools: it reads `sessions` (every session but yours, archived ones included, oldest first), `projects` and `now()`, and returns the rollup.
For what conversations say: `await mentions(query, { since })` gives { [sessionId]: time of its latest matching message } over every session's user and assistant messages (subagents included; not tool output or thinking), from the search index in one query. Query: words, all required; \"a phrase\"; prefix*. Words, not substrings: \"p2\" matches \"apps/p2\" and \"P2\", not \"p2x\".
For a recent mention, return { id, at } with `at` the mention's time and a window (\"1d\") rather than since + now(): the window keeps the cutoff current without reading now().
A member is a session ID, or { id, at, keep }: `at` is the time the window compares and the list sorts by (default the session's updatedAt), `keep` shows it whatever its time.
Archived sessions are never shown, and unknown IDs are dropped. Pinned, open, working, unread and failed sessions always show; a group shows while it has a session to show, or with keep.
window (default \"3d\"): \"1d\" | \"3d\" | \"1w\" | \"forever\", which the owner can change, or \"none\": every member shows.
refresh (default { kind: \"auto\" }): auto (runs again when a field the script read changed), { kind: \"every\", minutes } (on that interval, when something it read changed), or manual. The owner can refresh any rollup by hand.
Read only the fields you need: reading now() makes every refresh run the script. A script that calls mentions runs again when what the call answers changes.";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolArgs {
    action: String,
    key: Option<String>,
    name: Option<String>,
    window: Option<Value>,
    refresh: Option<Value>,
    code: Option<String>,
}

fn describe(entry: &Value) -> String {
    let def = &entry["definition"];
    let key = def["key"].as_str().unwrap_or("");
    let name = &def["name"];
    let window = def["window"].as_str().unwrap_or("");
    let refresh = &def["refresh"];
    let every = if refresh["kind"].as_str() == Some("every") {
        format!("every {} min", refresh["minutes"].as_u64().unwrap_or(1))
    } else {
        refresh["kind"].as_str().unwrap_or("auto").to_owned()
    };
    let mut lines = vec![format!(
        "{key} · {name} · window {window} · refresh {every}"
    )];
    if let Some(error) = entry["error"].as_str() {
        lines.push(format!("Last run failed: {error}"));
    }
    if !entry["output"].is_null() {
        let reads = entry["reads"]
            .as_array()
            .map(|arr| {
                let items: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
                if items.is_empty() {
                    "nothing".to_owned()
                } else {
                    items.join(", ")
                }
            })
            .unwrap_or_else(|| "nothing".to_owned());
        lines.push(format!(
            "Output: {}; reads {reads}",
            summary(&entry["output"])
        ));
    } else {
        lines.push("No output yet.".to_owned());
    }
    lines.push("```js".to_owned());
    lines.push(def["code"].as_str().unwrap_or("").to_owned());
    lines.push("```".to_owned());
    lines.join("\n")
}

fn summary(output: &Value) -> String {
    let layout = output["layout"].as_str().unwrap_or("");
    if layout == "groups" {
        let groups = output["groups"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let list: Vec<String> = groups
            .iter()
            .map(|g| {
                let name = g["name"].as_str().unwrap_or("");
                let count = g["members"].as_array().map_or(0, Vec::len);
                format!("{name}: {count}")
            })
            .collect();
        return format!("{} groups ({})", groups.len(), list.join(", "));
    }
    let count = output["members"].as_array().map_or(0, Vec::len);
    format!("{layout}, {count} sessions")
}

fn preview_text(preview: &Value) -> Result<String> {
    if preview["ok"].as_bool() != Some(true) {
        let msg = preview["error"].as_str().unwrap_or("Preview failed");
        return Err(pi_rs_chord::Error::Invalid(msg.to_owned()));
    }
    let reads = preview["reads"]
        .as_array()
        .map(|arr| {
            let items: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
            if items.is_empty() {
                "nothing".to_owned()
            } else {
                items.join(", ")
            }
        })
        .unwrap_or_else(|| "nothing".to_owned());
    let json_text: String = serde_json::to_string(&preview["output"])?
        .chars()
        .take(4000)
        .collect();
    Ok(format!(
        "Output: {}\nReads: {reads}\n{json_text}",
        summary(&preview["output"])
    ))
}

async fn act(
    args: &ToolArgs,
    host: &Host,
    contract: Arc<pi_rs_chord::contracts::Contract>,
    context: &Context,
) -> Result<String> {
    let root = host.root();
    let rollups = root.hold(contract, JsonValue::Object(Default::default()))?;
    rollups.wait(context).await?;
    match args.action.as_str() {
        "list" => {
            let entries = rollups
                .call("list", JsonValue::Null, context)
                .await?
                .to_serde()?;
            let arr = entries.as_array().map(Vec::as_slice).unwrap_or(&[]);
            if arr.is_empty() {
                Ok("No rollups made from code yet.".to_owned())
            } else {
                Ok(arr.iter().map(describe).collect::<Vec<_>>().join("\n\n"))
            }
        }
        "preview" => {
            let code = args
                .code
                .as_deref()
                .filter(|c| !c.is_empty())
                .ok_or_else(|| pi_rs_chord::Error::Invalid("preview needs code".into()))?;
            let result = rollups
                .call(
                    "preview",
                    JsonValue::from_serde(json!({ "code": code })),
                    context,
                )
                .await?
                .to_serde()?;
            preview_text(&result)
        }
        "save" => {
            let (Some(key), Some(name), Some(code)) = (
                args.key.as_deref().filter(|s| !s.is_empty()),
                args.name.as_deref().filter(|s| !s.is_empty()),
                args.code.as_deref().filter(|s| !s.is_empty()),
            ) else {
                return Err(pi_rs_chord::Error::Invalid(
                    "save needs key, name and code".into(),
                ));
            };
            let entry = rollups
                .call(
                    "save",
                    JsonValue::from_serde(json!({
                        "key": key,
                        "name": name,
                        "code": code,
                        "window": args.window.clone().unwrap_or_else(|| json!("3d")),
                        "refresh": args.refresh.clone().unwrap_or_else(|| json!({ "kind": "auto" })),
                    })),
                    context,
                )
                .await?
                .to_serde()?;
            Ok(format!("Saved.\n{}", describe(&entry)))
        }
        "refresh" => {
            let key = args
                .key
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| pi_rs_chord::Error::Invalid("refresh needs a key".into()))?;
            let entry = rollups
                .call(
                    "refresh",
                    JsonValue::from_serde(json!({ "key": key })),
                    context,
                )
                .await?
                .to_serde()?;
            Ok(describe(&entry))
        }
        "remove" => {
            let key = args
                .key
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| pi_rs_chord::Error::Invalid("remove needs a key".into()))?;
            rollups
                .call(
                    "remove",
                    JsonValue::from_serde(json!({ "key": key })),
                    context,
                )
                .await?;
            Ok(format!("Removed {key}."))
        }
        other => Err(pi_rs_chord::Error::Invalid(format!(
            "Unknown action: {other}"
        ))),
    }
}

pub fn rollups_extension(
    host: Host,
    worker: Arc<WorkerProcess>,
    contract: Arc<pi_rs_chord::contracts::Contract>,
) -> Extension {
    let tool = define_tool(ToolRegistration {
        declaration: Tool {
            name: ROLLUPS_TOOL.into(),
            description: DESCRIPTION.into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "preview", "save", "refresh", "remove"] },
                    "key": { "type": "string" },
                    "name": { "type": "string" },
                    "window": { "type": "string", "enum": ["1d", "3d", "1w", "forever", "none"] },
                    "refresh": { "type": "object" },
                    "code": { "type": "string" }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            constrained_sampling: None,
        },
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: None,
        output_limits: None,
        replay_safe: true,
        callers: None,
        deferred: false,
        execute: Arc::new(move |args_val, _, context| {
            let host = host.clone();
            let worker = worker.clone();
            let contract = contract.clone();
            Box::pin(async move {
                let args: ToolArgs = serde_json::from_value(args_val).map_err(|e| {
                    DurableError::invalid(format!("Invalid sidebar_rollups args: {e}"))
                })?;
                let mut details = Map::new();
                details.insert("action".into(), json!(args.action));
                if let Some(key) = &args.key {
                    details.insert("key".into(), json!(key));
                }
                let result = worker.call(act(&args, &host, contract, context)).await;
                match result {
                    Ok(text) => Ok(ToolResult {
                        output: Some(vec![InputContent::Text(text.into())]),
                        details: Some(Value::Object(details)),
                        is_error: Some(false),
                        ..Default::default()
                    }
                    .into()),
                    Err(err) => {
                        if context.is_aborted() {
                            return Err(DurableError::invalid(err.to_string()));
                        }
                        Ok(ToolResult {
                            output: Some(vec![InputContent::Text(err.to_string().into())]),
                            details: Some(Value::Object(details)),
                            is_error: Some(true),
                            ..Default::default()
                        }
                        .into())
                    }
                }
            })
        }),
    });
    define_extension(Extension {
        name: "p2-rollups".into(),
        tools: vec![tool],
        ..Default::default()
    })
}

pub fn worker_facet() -> Facet {
    define_facet(|host| {
        let chord_host = host.host();
        let worker = worker_of(&chord_host)?;
        if worker
            .init
            .values
            .get(FAMILIAR_INIT)
            .and_then(Value::as_bool)
            != Some(true)
        {
            return Ok(());
        }
        let contract = Arc::new(lookup("p2.sidebar-rollups")?);
        let extension = rollups_extension(chord_host, worker.clone(), contract);
        let mut access = IndexMap::new();
        access.insert(
            ROLLUPS_TOOL.to_owned(),
            ToolAccess {
                main_only: true,
                ..Default::default()
            },
        );
        worker
            .registries
            .extensions
            .install_with_access(extension, &access)?;
        Ok(())
    })
}
