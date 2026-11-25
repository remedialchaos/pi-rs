//! The agent's `view_*` code-mode tools (`p2-views` extension).

use std::{
    collections::HashSet,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use pi_rs_ai::{InputContent, Tool};
use pi_rs_chord::context::Context;
use pi_rs_durable::{
    Session,
    harness::{Extension, ToolRegistration},
    session::Error as DurableError,
    tools::ToolResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    contract::{TRANSCRIPT, View, is_valid_view_id, is_valid_view_kind},
    doc::{
        ViewSpec, describe_annotation, free_view_id, open_view, read_views, remove_view,
        update_views_doc, view_name,
    },
    layout::arrangement_error,
};

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewDetails {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

fn details_of(action: &str, view: &View) -> ViewDetails {
    ViewDetails {
        action: action.to_string(),
        id: Some(view.id.clone()),
        title: Some(view.title.clone()),
        kind: Some(view.kind.clone()),
    }
}

fn ok_result(text: String, value: Value, details: ViewDetails) -> ToolResult {
    ToolResult {
        output: Some(vec![InputContent::Text(text.into())]),
        structured_output: Some(value),
        details: Some(serde_json::to_value(details).unwrap_or(Value::Null)),
        ..Default::default()
    }
}

fn err_result(action: &str, msg: String) -> ToolResult {
    ToolResult {
        output: Some(vec![InputContent::Text(msg.into())]),
        is_error: Some(true),
        details: Some(json!({ "action": action })),
        ..Default::default()
    }
}

fn make_tool(
    name: &str,
    description: &str,
    parameters: Value,
    replay_safe: bool,
    run: impl Fn(Value, Session, Context) -> pi_rs_chord::sync::SyncFuture<(String, Value, ViewDetails)>
    + Send
    + Sync
    + 'static,
) -> ToolRegistration {
    let tool_name = name.to_string();
    let run = Arc::new(run);
    ToolRegistration {
        declaration: Tool {
            name: tool_name.clone(),
            description: description.to_string(),
            parameters,
            constrained_sampling: None,
        },
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: None,
        output_limits: None,
        execute: Arc::new(move |args, invocation, ctx| {
            let run = run.clone();
            let tool_name = tool_name.clone();
            let session = invocation.harness().session.clone();
            let ctx = ctx.clone();
            Box::pin(async move {
                match run(args, session, ctx).await {
                    Ok((text, value, details)) => Ok(ok_result(text, value, details).into()),
                    Err(e) => Ok(err_result(&tool_name, e.to_string()).into()),
                }
            })
        }),
        replay_safe,
        callers: Some(vec!["tools".to_string()]),
        deferred: false,
    }
}

pub fn create_views_extension() -> Extension {
    let view_list = make_tool(
        "view_list",
        "The session's views (id, kind, item, title), the layout you arranged last, the person's selections and the annotations nobody resolved.",
        json!({ "type": "object", "properties": {} }),
        true,
        |_args, session, ctx| {
            Box::pin(async move {
                let state = read_views(&session, &ctx).await?;
                let views_json: Vec<Value> = state
                    .views
                    .iter()
                    .map(|v| {
                        let mut obj = json!({
                            "id": v.id,
                            "kind": v.kind,
                            "title": v.title,
                        });
                        if let Some(item) = &v.item {
                            obj["item"] = json!(item);
                        }
                        obj
                    })
                    .collect();

                let mut selections = Vec::new();
                for v in &state.views {
                    if let Some(sel) = &v.selection {
                        let mut s = serde_json::to_value(sel).unwrap_or_default();
                        if let Some(m) = s.as_object_mut() {
                            m.insert("view".into(), json!(v.id));
                        }
                        selections.push(s);
                    }
                }
                for each in state.inline.as_deref().unwrap_or(&[]) {
                    let mut s = serde_json::to_value(&each.selection).unwrap_or_default();
                    if let Some(m) = s.as_object_mut() {
                        m.insert("view".into(), json!(TRANSCRIPT));
                        m.insert("kind".into(), json!(each.kind));
                        m.insert("item".into(), json!(each.item));
                    }
                    selections.push(s);
                }

                let annotations: Vec<Value> = state
                    .annotations
                    .iter()
                    .map(|a| {
                        let mut v = serde_json::to_value(a).unwrap_or_default();
                        if let Some(m) = v.as_object_mut() {
                            m.insert("text".into(), json!(describe_annotation(&state.views, a)));
                        }
                        v
                    })
                    .collect();

                let mut value = json!({
                    "views": views_json,
                    "selections": selections,
                    "annotations": annotations,
                });
                if let Some(arr) = &state.arrangement {
                    value["arrangement"] = serde_json::to_value(arr).unwrap_or_default();
                }

                let text = if state.views.is_empty() {
                    "No views are open.".to_string()
                } else {
                    state
                        .views
                        .iter()
                        .map(|v| format!("- {}", view_name(&state.views, &v.id)))
                        .collect::<Vec<_>>()
                        .join("\n")
                };

                Ok((
                    text,
                    value,
                    ViewDetails {
                        action: "list".into(),
                        id: None,
                        title: None,
                        kind: None,
                    },
                ))
            })
        },
    );

    let view_open = make_tool(
        "view_open",
        "Opens a view beside the transcript (or, with the id of one, shows it again with what you give): kind markdown (item: a file), changes (item: an optional path), or a plugin's kind (item: its item ID). Returns its id.",
        json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "pattern": "^[a-z0-9][a-z0-9._-]{0,63}$" },
                "item": { "type": "string", "minLength": 1, "maxLength": 4096 },
                "title": { "type": "string", "minLength": 1, "maxLength": 120 },
                "id": { "type": "string", "pattern": "^[a-z0-9][a-z0-9_-]{0,63}$" }
            },
            "required": ["kind"]
        }),
        false,
        |args, session, ctx| {
            Box::pin(async move {
                let kind = args
                    .get("kind")
                    .and_then(Value::as_str)
                    .filter(|k| is_valid_view_kind(k))
                    .ok_or_else(|| pi_rs_chord::Error::Invalid("invalid view kind".into()))?
                    .to_string();
                let item = args.get("item").and_then(Value::as_str).map(str::to_string);
                let title_arg = args
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let asked_id = args.get("id").and_then(Value::as_str).map(str::to_string);
                if let Some(id) = &asked_id {
                    if id == TRANSCRIPT {
                        return Err(pi_rs_chord::Error::Invalid(
                            "id: not transcript (the chat is no view)".into(),
                        ));
                    }
                    if !is_valid_view_id(id) {
                        return Err(pi_rs_chord::Error::Invalid("invalid view id".into()));
                    }
                }

                let state = read_views(&session, &ctx).await?;
                let existing = asked_id
                    .as_deref()
                    .and_then(|id| state.views.iter().find(|v| v.id == id));
                if let Some(ex) = existing
                    && ex.kind != kind
                {
                    return Err(pi_rs_chord::Error::Invalid(format!(
                        "View \"{}\" is a {} view, not {kind}",
                        ex.id, ex.kind
                    )));
                }
                let final_item = item.or_else(|| existing.and_then(|ex| ex.item.clone()));
                if kind == "markdown" && final_item.is_none() {
                    return Err(pi_rs_chord::Error::Invalid(
                        "A markdown view needs item (the file)".into(),
                    ));
                }
                let raw_title = title_arg
                    .or_else(|| existing.map(|ex| ex.title.clone()))
                    .unwrap_or_else(|| {
                        if kind == "changes" {
                            match &final_item {
                                Some(it) => format!("Changes in {it}"),
                                None => "Changes".to_string(),
                            }
                        } else {
                            final_item.clone().unwrap_or_else(|| kind.clone())
                        }
                    });
                let title: String = raw_title.chars().take(120).collect();
                let id = asked_id.unwrap_or_else(|| free_view_id(&state, &title, &kind));
                let now = now_millis();

                let view = session
                    .commit(
                        async move |tx| {
                            update_views_doc(tx, |draft| {
                                open_view(
                                    draft,
                                    ViewSpec {
                                        id,
                                        kind,
                                        title,
                                        item: final_item,
                                    },
                                    now,
                                )
                            })
                            .await
                            .map_err(|e| DurableError::invalid(e.to_string()))
                        },
                        &ctx,
                    )
                    .await
                    .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?;

                let text = format!(
                    "Opened {}. The person sees it beside the transcript.",
                    view_name(std::slice::from_ref(&view), &view.id)
                );
                let details = details_of("open", &view);
                Ok((text, json!({ "id": view.id }), details))
            })
        },
    );

    let view_close = make_tool(
        "view_close",
        "Closes a view (its annotations go with it).",
        json!({
            "type": "object",
            "properties": { "id": { "type": "string", "maxLength": 64 } },
            "required": ["id"]
        }),
        true,
        |args, session, ctx| {
            Box::pin(async move {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| pi_rs_chord::Error::Invalid("missing view id".into()))?
                    .to_string();
                let state = read_views(&session, &ctx).await?;
                let view = state
                    .views
                    .into_iter()
                    .find(|v| v.id == id)
                    .ok_or_else(|| pi_rs_chord::Error::Invalid(format!("No view \"{id}\"")))?;
                let id_clone = id.clone();
                session
                    .commit(
                        async move |tx| {
                            update_views_doc(tx, |draft| {
                                remove_view(draft, &id_clone);
                                Ok(())
                            })
                            .await
                            .map_err(|e| DurableError::invalid(e.to_string()))
                        },
                        &ctx,
                    )
                    .await
                    .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?;

                Ok((
                    format!("Closed view \"{id}\"."),
                    json!({ "closed": id }),
                    details_of("close", &view),
                ))
            })
        },
    );

    let view_arrange = make_tool(
        "view_arrange",
        "Lays out the person's panes: a node is a view ID ('transcript' is the chat), {tabs: [IDs], active: ID} or {row or column: [nodes], sizes: [shares]}; views left out are hidden.",
        json!({
            "type": "object",
            "properties": { "layout": {} },
            "required": ["layout"]
        }),
        true,
        |args, session, ctx| {
            Box::pin(async move {
                let layout = args
                    .get("layout")
                    .cloned()
                    .ok_or_else(|| pi_rs_chord::Error::Invalid("missing layout".into()))?;
                let state = read_views(&session, &ctx).await?;
                let mut known: HashSet<String> = state.views.into_iter().map(|v| v.id).collect();
                known.insert(TRANSCRIPT.into());
                if let Some(err) = arrangement_error(&layout, &known) {
                    return Err(pi_rs_chord::Error::Invalid(format!("layout: {err}")));
                }
                let stamp = now_millis();
                let layout_clone = layout.clone();
                session
                    .commit(
                        async move |tx| {
                            update_views_doc(tx, |draft| {
                                draft.arrangement = Some(super::contract::Arrangement {
                                    layout: layout_clone,
                                    stamp,
                                });
                                Ok(())
                            })
                            .await
                            .map_err(|e| DurableError::invalid(e.to_string()))
                        },
                        &ctx,
                    )
                    .await
                    .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?;

                Ok((
                    "Arranged the person's panes; they may rearrange them.".to_string(),
                    json!({ "arranged": layout }),
                    ViewDetails {
                        action: "arrange".into(),
                        id: None,
                        title: None,
                        kind: None,
                    },
                ))
            })
        },
    );

    let view_resolve = make_tool(
        "view_resolve",
        "Removes annotations you dealt with: these IDs, or every one on a view ('transcript' for the chat's).",
        json!({
            "type": "object",
            "properties": {
                "annotations": { "type": "array", "items": { "type": "string" }, "maxItems": 200 },
                "view": { "type": "string", "maxLength": 64 }
            }
        }),
        true,
        |args, session, ctx| {
            Box::pin(async move {
                let ids: HashSet<String> = args
                    .get("annotations")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let view = args.get("view").and_then(Value::as_str).map(str::to_string);
                if ids.is_empty() && view.is_none() {
                    return Err(pi_rs_chord::Error::Invalid(
                        "Give annotations (ids) or view (all of a view's)".into(),
                    ));
                }
                let removed = session
                    .commit(
                        async move |tx| {
                            update_views_doc(tx, |draft| {
                                let before = draft.annotations.len();
                                draft.annotations.retain(|a| {
                                    !ids.contains(&a.id)
                                        && view.as_deref().is_none_or(|v| a.view != v)
                                });
                                Ok(before - draft.annotations.len())
                            })
                            .await
                            .map_err(|e| DurableError::invalid(e.to_string()))
                        },
                        &ctx,
                    )
                    .await
                    .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?;

                let plural = if removed == 1 { "" } else { "s" };
                Ok((
                    format!("Resolved {removed} annotation{plural}."),
                    json!({ "resolved": removed }),
                    ViewDetails {
                        action: "resolve".into(),
                        id: None,
                        title: None,
                        kind: None,
                    },
                ))
            })
        },
    );

    Extension {
        name: "p2-views".into(),
        hooks: Vec::new(),
        wraps: Vec::new(),
        tasks: Vec::new(),
        tools: vec![view_list, view_open, view_close, view_arrange, view_resolve],
        sections: Vec::new(),
    }
}
