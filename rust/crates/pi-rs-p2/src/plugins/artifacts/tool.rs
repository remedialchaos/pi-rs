//! The agent's 12 artifact tools (code mode only, namespace `artifact`).

use std::{
    collections::HashSet,
    future::Future,
    pin::Pin,
    sync::{Arc, LazyLock},
    time::{SystemTime, UNIX_EPOCH},
};

use pi_rs_ai::{InputContent, Tool};
use pi_rs_chord::{Error, Result, context::Context, delta::Segment};
use pi_rs_durable::{
    DocDefinition, DocToken,
    documents::{DocumentSemantics, InitialDocument, resolve_address},
    harness::{Extension, Invocation, ToolRegistration},
    session::Error as DurableError,
    tools::ToolResult,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use unicode_normalization::UnicodeNormalization;

use super::{
    pages::{EvalResult, LogEntry},
    store::{
        ARTIFACT_ID_PATTERN, ArtifactEdit, ArtifactKind, ArtifactRead, ArtifactSummary,
        MAX_ARTIFACT_STATE, WriteInput,
    },
};

pub type BoxBackendFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// What the tools reach: the session's `Artifacts` on the server (its worker variant).
pub trait ArtifactBackend: Send + Sync {
    fn list<'a>(&'a self, context: &'a Context) -> BoxBackendFuture<'a, Vec<ArtifactSummary>>;
    fn read<'a>(
        &'a self,
        id: &'a str,
        version: Option<i64>,
        context: &'a Context,
    ) -> BoxBackendFuture<'a, ArtifactRead>;
    fn write<'a>(
        &'a self,
        input: WriteInput,
        context: &'a Context,
    ) -> BoxBackendFuture<'a, (String, i64)>;
    fn edit<'a>(
        &'a self,
        id: &'a str,
        edits: Vec<ArtifactEdit>,
        context: &'a Context,
    ) -> BoxBackendFuture<'a, i64>;
    fn remove<'a>(&'a self, id: &'a str, context: &'a Context) -> BoxBackendFuture<'a, ()>;
    fn state<'a>(&'a self, id: &'a str, context: &'a Context) -> BoxBackendFuture<'a, Value>;
    fn set_state<'a>(
        &'a self,
        id: &'a str,
        state: Value,
        context: &'a Context,
    ) -> BoxBackendFuture<'a, ()>;
    fn errors<'a>(
        &'a self,
        id: &'a str,
        context: &'a Context,
    ) -> BoxBackendFuture<'a, Vec<LogEntry>>;
    fn logs<'a>(
        &'a self,
        id: &'a str,
        since: Option<i64>,
        context: &'a Context,
    ) -> BoxBackendFuture<'a, (Vec<LogEntry>, i64)>;
    fn run<'a>(
        &'a self,
        id: &'a str,
        code: &'a str,
        context: &'a Context,
    ) -> BoxBackendFuture<'a, EvalResult>;
}

const VIEWS_DOC_KIND: &str = "p2.views";
const TRANSCRIPT_VIEW_ID: &str = "transcript";
static VIEW_ID_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: static regex literal verified at compile/test time.
    Regex::new("^[a-z0-9][a-z0-9_-]{0,63}$").expect("valid view id regex")
});

fn views_doc_token() -> DocToken {
    Arc::new(DocDefinition {
        kind: VIEWS_DOC_KIND.into(),
        version: 1,
        semantics: DocumentSemantics::Session,
        initial: InitialDocument::Singleton(Arc::new(|| {
            Ok(Map::from_iter([
                ("views".into(), Value::Array(Vec::new())),
                ("annotations".into(), Value::Array(Vec::new())),
            ]))
        })),
        migrate: None,
        checkpoint_when: Arc::new(|_, _, info| info.deltas_since_base >= 32),
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ViewsDoc {
    #[serde(default)]
    views: Vec<ViewEntry>,
    #[serde(default)]
    annotations: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inline: Option<Vec<Value>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ViewEntry {
    id: String,
    kind: String,
    title: String,
    focus: i64,
    created: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selection: Option<Value>,
}

fn free_view_id(views: &[ViewEntry], title: &str, kind: &str) -> String {
    let normalized: String = title.to_lowercase().nfkd().collect();
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in normalized.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
    }
    let trimmed: String = slug.trim_matches('-').chars().take(48).collect();
    let candidate = if trimmed.is_empty() {
        kind.to_owned()
    } else {
        trimmed
    };
    let base = if VIEW_ID_REGEX.is_match(&candidate) && candidate != TRANSCRIPT_VIEW_ID {
        candidate
    } else {
        format!("view-{candidate}").chars().take(48).collect()
    };
    let taken: HashSet<&str> = views.iter().map(|v| v.id.as_str()).collect();
    if !taken.contains(base.as_str()) {
        return base;
    }
    for n in 2usize.. {
        let next = format!("{base}-{n}");
        if !taken.contains(next.as_str()) {
            return next;
        }
    }
    base
}

fn id_schema() -> Value {
    json!({ "type": "string", "pattern": ARTIFACT_ID_PATTERN })
}

fn kind_schema() -> Value {
    json!({ "enum": ["html", "md"] })
}

fn summary_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["id", "kind", "title", "version", "updated"],
        "properties": {
            "id": id_schema(),
            "kind": kind_schema(),
            "title": { "type": "string", "minLength": 1, "maxLength": 200 },
            "version": { "type": "integer", "minimum": 1 },
            "updated": { "type": "number" }
        }
    })
}

fn log_entry_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["seq", "level", "text", "at"],
        "properties": {
            "seq": { "type": "integer" },
            "level": { "type": "string" },
            "text": { "type": "string" },
            "at": { "type": "number" }
        }
    })
}

struct ToolOutcome {
    value: Value,
    text: String,
    details: Value,
}

fn make_tool<F>(
    name: &str,
    description: &str,
    parameters: Value,
    output_schema: Value,
    replay_safe: bool,
    run: F,
) -> ToolRegistration
where
    F: for<'a> Fn(
            Value,
            &'a Invocation,
            &'a Context,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutcome>> + Send + 'a>>
        + Send
        + Sync
        + 'static,
{
    let run = Arc::new(run);
    ToolRegistration {
        declaration: Tool {
            name: name.to_owned(),
            description: description.to_owned(),
            parameters,
            constrained_sampling: None,
        },
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: Some(output_schema),
        output_limits: None,
        replay_safe,
        callers: Some(vec!["tools".into()]),
        deferred: false,
        execute: Arc::new(move |args, invocation, context| {
            let run = Arc::clone(&run);
            Box::pin(async move {
                match run(args, invocation, context).await {
                    Ok(outcome) => Ok(ToolResult {
                        output: Some(vec![InputContent::Text(outcome.text.into())]),
                        structured_output: Some(outcome.value),
                        details: Some(outcome.details),
                        is_error: None,
                        ..Default::default()
                    }
                    .into()),
                    Err(err) => {
                        if context.is_aborted() {
                            return Err(DurableError::invalid(err.to_string()));
                        }
                        Ok(ToolResult {
                            output: Some(vec![InputContent::Text(err.to_string().into())]),
                            is_error: Some(true),
                            ..Default::default()
                        }
                        .into())
                    }
                }
            })
        }),
    }
}

async fn ensure_html(backend: &dyn ArtifactBackend, id: &str, context: &Context) -> Result<()> {
    let read = backend.read(id, None, context).await?;
    if read.summary.kind != ArtifactKind::Html {
        return Err(Error::Invalid(format!(
            "Artifact \"{id}\" is {}: only html pages run",
            read.summary.kind.as_str()
        )));
    }
    Ok(())
}

/// Creates the `p2-artifacts` Durable extension exposing all 12 `artifact_*` tools.
pub fn create_artifact_extension(backend: Arc<dyn ArtifactBackend>) -> Extension {
    let id_args = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["id"],
        "properties": { "id": id_schema() }
    });

    let mut tools = Vec::with_capacity(12);

    // 1. artifact_list
    {
        let backend = Arc::clone(&backend);
        tools.push(make_tool(
            "artifact_list",
            "The session's artifacts (id, kind, title, newest version, updated), the newest first.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "kind": kind_schema() }
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["artifacts"],
                "properties": { "artifacts": { "type": "array", "items": summary_schema() } }
            }),
            true,
            move |args, _inv, ctx| {
                let backend = Arc::clone(&backend);
                Box::pin(async move {
                    #[derive(Deserialize)]
                    struct Args {
                        #[serde(default)]
                        kind: Option<ArtifactKind>,
                    }
                    let parsed: Args = serde_json::from_value(args)?;
                    let all = backend.list(ctx).await?;
                    let filtered: Vec<ArtifactSummary> = all
                        .into_iter()
                        .filter(|each| parsed.kind.is_none_or(|k| each.kind == k))
                        .collect();
                    let text = if filtered.is_empty() {
                        "No artifacts.".to_owned()
                    } else {
                        filtered
                            .iter()
                            .map(|each| {
                                format!(
                                    "- {} ({}, version {}): {}",
                                    each.id,
                                    each.kind.as_str(),
                                    each.version,
                                    each.title
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    Ok(ToolOutcome {
                        value: json!({ "artifacts": filtered }),
                        text,
                        details: json!({}),
                    })
                })
            },
        ));
    }

    // 2. artifact_read
    {
        let backend = Arc::clone(&backend);
        tools.push(make_tool(
            "artifact_read",
            "An artifact's content: its newest version, or `version`.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": id_schema(),
                    "version": { "type": "integer", "minimum": 1 }
                }
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id", "kind", "title", "version", "content"],
                "properties": {
                    "id": { "type": "string" },
                    "kind": { "type": "string" },
                    "title": { "type": "string" },
                    "version": { "type": "integer" },
                    "content": { "type": "string" }
                }
            }),
            true,
            move |args, _inv, ctx| {
                let backend = Arc::clone(&backend);
                Box::pin(async move {
                    #[derive(Deserialize)]
                    struct Args {
                        id: String,
                        #[serde(default)]
                        version: Option<i64>,
                    }
                    let parsed: Args = serde_json::from_value(args)?;
                    let read = backend.read(&parsed.id, parsed.version, ctx).await?;
                    Ok(ToolOutcome {
                        value: json!({
                            "id": parsed.id,
                            "kind": read.summary.kind,
                            "title": read.summary.title,
                            "version": read.version,
                            "content": read.content,
                        }),
                        text: read.content,
                        details: json!({
                            "kind": read.summary.kind,
                            "id": parsed.id,
                            "version": read.version,
                        }),
                    })
                })
            },
        ));
    }

    // 3. artifact_write
    {
        let backend = Arc::clone(&backend);
        tools.push(make_tool(
            "artifact_write",
            "Writes an artifact: a new one (kind html: a page; md: a Markdown document; its id made from the title unless given), or the next version of artifact `id`. Open pages of it reload. Returns its id and version.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["kind", "title", "content"],
                "properties": {
                    "id": id_schema(),
                    "kind": kind_schema(),
                    "title": { "type": "string", "minLength": 1, "maxLength": 200 },
                    "content": { "type": "string" }
                }
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id", "version"],
                "properties": { "id": { "type": "string" }, "version": { "type": "integer" } }
            }),
            false,
            move |args, _inv, ctx| {
                let backend = Arc::clone(&backend);
                Box::pin(async move {
                    let input: WriteInput = serde_json::from_value(args)?;
                    let kind = input.kind;
                    let (id, version) = backend.write(input, ctx).await?;
                    Ok(ToolOutcome {
                        value: json!({ "id": id, "version": version }),
                        text: format!(
                            "Wrote {} artifact \"{}\" (version {}).",
                            kind.as_str(),
                            id,
                            version
                        ),
                        details: json!({ "kind": kind, "id": id, "version": version }),
                    })
                })
            },
        ));
    }

    // 4. artifact_edit
    {
        let backend = Arc::clone(&backend);
        tools.push(make_tool(
            "artifact_edit",
            "Edits an artifact's newest content by exact replacements (each `old` occurring exactly once, none overlapping) into a new version. Returns the version.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id", "edits"],
                "properties": {
                    "id": id_schema(),
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 100,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["old", "new"],
                            "properties": {
                                "old": { "type": "string", "minLength": 1 },
                                "new": { "type": "string" }
                            }
                        }
                    }
                }
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["version"],
                "properties": { "version": { "type": "integer" } }
            }),
            false,
            move |args, _inv, ctx| {
                let backend = Arc::clone(&backend);
                Box::pin(async move {
                    #[derive(Deserialize)]
                    struct Args {
                        id: String,
                        edits: Vec<ArtifactEdit>,
                    }
                    let parsed: Args = serde_json::from_value(args)?;
                    let version = backend.edit(&parsed.id, parsed.edits, ctx).await?;
                    Ok(ToolOutcome {
                        value: json!({ "version": version }),
                        text: format!("Edited artifact \"{}\" (version {}).", parsed.id, version),
                        details: json!({ "id": parsed.id, "version": version }),
                    })
                })
            },
        ));
    }

    // 5. artifact_show
    {
        let backend = Arc::clone(&backend);
        tools.push(make_tool(
            "artifact_show",
            "Shows an artifact in the transcript, right after this call, for everyone in the session (a page runs there).",
            id_args.clone(),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id", "kind"],
                "properties": { "id": { "type": "string" }, "kind": { "type": "string" } }
            }),
            true,
            move |args, _inv, ctx| {
                let backend = Arc::clone(&backend);
                Box::pin(async move {
                    #[derive(Deserialize)]
                    struct Args {
                        id: String,
                    }
                    let parsed: Args = serde_json::from_value(args)?;
                    let read = backend.read(&parsed.id, None, ctx).await?;
                    Ok(ToolOutcome {
                        value: json!({ "id": parsed.id, "kind": read.summary.kind }),
                        text: format!(
                            "Showed artifact \"{}\" ({}) in the transcript.",
                            parsed.id, read.summary.title
                        ),
                        details: json!({ "kind": read.summary.kind, "id": parsed.id }),
                    })
                })
            },
        ));
    }

    // 6. artifact_open
    {
        let backend = Arc::clone(&backend);
        tools.push(make_tool(
            "artifact_open",
            "Opens an artifact beside the chat (a view of its kind), or shows its view again. Returns the view's id.",
            id_args.clone(),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["view"],
                "properties": { "view": { "type": "string" } }
            }),
            false,
            move |args, inv, ctx| {
                let backend = Arc::clone(&backend);
                Box::pin(async move {
                    #[derive(Deserialize)]
                    struct Args {
                        id: String,
                    }
                    let parsed: Args = serde_json::from_value(args)?;
                    let read = backend.read(&parsed.id, None, ctx).await?;
                    let doc_token = views_doc_token();
                    let addr = resolve_address(&doc_token, None, None)
                        .map_err(|e| Error::Invalid(e.to_string()))?;
                    let session = &inv.harness().session;
                    let now = now_ms();
                    let kind_str = read.summary.kind.as_str().to_owned();
                    let title: String = read.summary.title.chars().take(120).collect();
                    let item_id = parsed.id.clone();

                    let current_raw = session
                        .snapshot(&doc_token, &addr, ctx)
                        .await
                        .map_err(|e| Error::Invalid(e.to_string()))?;
                    let mut doc: ViewsDoc = match current_raw {
                        Some(raw) => serde_json::from_value(Value::Object((*raw).clone()))?,
                        None => ViewsDoc::default(),
                    };
                    doc.views.retain(|v| v.kind != "custom");
                    let view_id = doc
                        .views
                        .iter()
                        .find(|v| v.kind == kind_str && v.item.as_deref() == Some(&item_id))
                        .map(|v| v.id.clone())
                        .unwrap_or_else(|| free_view_id(&doc.views, &title, &kind_str));

                    if let Some(existing) = doc.views.iter_mut().find(|v| v.id == view_id) {
                        if existing.kind != kind_str {
                            return Err(Error::Invalid(format!(
                                "View \"{view_id}\" is a {} view",
                                existing.kind
                            )));
                        }
                        existing.title = title;
                        existing.focus = now;
                        existing.item = Some(item_id.clone());
                    } else {
                        doc.views.push(ViewEntry {
                            id: view_id.clone(),
                            kind: kind_str,
                            title,
                            focus: now,
                            created: now,
                            item: Some(item_id.clone()),
                            selection: None,
                        });
                    }
                    let views_value = serde_json::to_value(&doc.views)?;
                    session
                        .commit(
                            async |tx| {
                                tx.doc(&doc_token, &addr, None)
                                    .await?
                                    .set(&[Segment::Key("views".into())], views_value.into())?;
                                Ok(())
                            },
                            ctx,
                        )
                        .await
                        .map_err(|e| Error::Invalid(e.to_string()))?;

                    Ok(ToolOutcome {
                        value: json!({ "view": view_id }),
                        text: format!(
                            "Opened artifact \"{}\" beside the chat (view \"{view_id}\").",
                            parsed.id
                        ),
                        details: json!({ "kind": read.summary.kind, "id": parsed.id }),
                    })
                })
            },
        ));
    }

    // 7. artifact_delete
    {
        let backend = Arc::clone(&backend);
        tools.push(make_tool(
            "artifact_delete",
            "Deletes an artifact with every version and its state.",
            id_args.clone(),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["deleted"],
                "properties": { "deleted": { "type": "string" } }
            }),
            true,
            move |args, _inv, ctx| {
                let backend = Arc::clone(&backend);
                Box::pin(async move {
                    #[derive(Deserialize)]
                    struct Args {
                        id: String,
                    }
                    let parsed: Args = serde_json::from_value(args)?;
                    backend.remove(&parsed.id, ctx).await?;
                    Ok(ToolOutcome {
                        value: json!({ "deleted": parsed.id }),
                        text: format!("Deleted artifact \"{}\".", parsed.id),
                        details: json!({}),
                    })
                })
            },
        ));
    }

    // 8. artifact_html_errors
    {
        let backend = Arc::clone(&backend);
        tools.push(make_tool(
            "artifact_html_errors",
            "The errors the open pages of an html artifact reported (script errors, failed loads), oldest first.",
            id_args.clone(),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["errors"],
                "properties": { "errors": { "type": "array", "items": log_entry_schema() } }
            }),
            true,
            move |args, _inv, ctx| {
                let backend = Arc::clone(&backend);
                Box::pin(async move {
                    #[derive(Deserialize)]
                    struct Args {
                        id: String,
                    }
                    let parsed: Args = serde_json::from_value(args)?;
                    ensure_html(backend.as_ref(), &parsed.id, ctx).await?;
                    let errors = backend.errors(&parsed.id, ctx).await?;
                    let text = if errors.is_empty() {
                        "No errors.".to_owned()
                    } else {
                        errors
                            .iter()
                            .map(|each| format!("- {}", each.text))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    Ok(ToolOutcome {
                        value: json!({ "errors": errors }),
                        text,
                        details: json!({}),
                    })
                })
            },
        ));
    }

    // 9. artifact_html_logs
    {
        let backend = Arc::clone(&backend);
        tools.push(make_tool(
            "artifact_html_logs",
            "What the open pages of an html artifact logged (console lines, errors, warnings) after `since` (the `next` of an earlier call), and the newest `next`.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": id_schema(),
                    "since": { "type": "integer", "minimum": 0 }
                }
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["logs", "next"],
                "properties": {
                    "logs": { "type": "array", "items": log_entry_schema() },
                    "next": { "type": "integer" }
                }
            }),
            true,
            move |args, _inv, ctx| {
                let backend = Arc::clone(&backend);
                Box::pin(async move {
                    #[derive(Deserialize)]
                    struct Args {
                        id: String,
                        #[serde(default)]
                        since: Option<i64>,
                    }
                    let parsed: Args = serde_json::from_value(args)?;
                    ensure_html(backend.as_ref(), &parsed.id, ctx).await?;
                    let (logs, next) = backend.logs(&parsed.id, parsed.since, ctx).await?;
                    let text = if logs.is_empty() {
                        "Nothing logged.".to_owned()
                    } else {
                        logs.iter()
                            .map(|each| format!("[{}] {}", each.level, each.text))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    Ok(ToolOutcome {
                        value: json!({ "logs": logs, "next": next }),
                        text,
                        details: json!({}),
                    })
                })
            },
        ));
    }

    // 10. artifact_html_eval
    {
        let backend = Arc::clone(&backend);
        tools.push(make_tool(
            "artifact_html_eval",
            "Runs JavaScript in an html artifact's page open on some device, as an async function body (`return` a JSON value). Fails when no page is open.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id", "code"],
                "properties": {
                    "id": id_schema(),
                    "code": { "type": "string", "minLength": 1, "maxLength": 100000 }
                }
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["value"],
                "properties": { "value": {} }
            }),
            false,
            move |args, _inv, ctx| {
                let backend = Arc::clone(&backend);
                Box::pin(async move {
                    #[derive(Deserialize)]
                    struct Args {
                        id: String,
                        code: String,
                    }
                    let parsed: Args = serde_json::from_value(args)?;
                    ensure_html(backend.as_ref(), &parsed.id, ctx).await?;
                    match backend.run(&parsed.id, &parsed.code, ctx).await? {
                        EvalResult::Ok { value, .. } => {
                            let text = serde_json::to_string(&value)?;
                            Ok(ToolOutcome {
                                value: json!({ "value": value }),
                                text,
                                details: json!({}),
                            })
                        }
                        EvalResult::Err { error, .. } => Err(Error::Invalid(error)),
                    }
                })
            },
        ));
    }

    // 11. artifact_html_state
    {
        let backend = Arc::clone(&backend);
        tools.push(make_tool(
            "artifact_html_state",
            "An html artifact's whole state (null until something set it), as the page last saved it.",
            id_args,
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["state"],
                "properties": { "state": {} }
            }),
            true,
            move |args, _inv, ctx| {
                let backend = Arc::clone(&backend);
                Box::pin(async move {
                    #[derive(Deserialize)]
                    struct Args {
                        id: String,
                    }
                    let parsed: Args = serde_json::from_value(args)?;
                    let state = backend.state(&parsed.id, ctx).await?;
                    let serialized = serde_json::to_string(&state)?;
                    let len = serialized.encode_utf16().count();
                    Ok(ToolOutcome {
                        value: json!({ "state": state }),
                        text: format!("State of \"{}\": {len} characters of JSON.", parsed.id),
                        details: json!({}),
                    })
                })
            },
        ));
    }

    // 12. artifact_html_set_state
    {
        let backend = Arc::clone(&backend);
        tools.push(make_tool(
            "artifact_html_set_state",
            &format!(
                "Replaces an html artifact's state (any JSON, at most {} KiB); its open pages get it at once.",
                MAX_ARTIFACT_STATE / 1024
            ),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id", "state"],
                "properties": { "id": id_schema(), "state": {} }
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": { "id": { "type": "string" } }
            }),
            true,
            move |args, _inv, ctx| {
                let backend = Arc::clone(&backend);
                Box::pin(async move {
                    #[derive(Deserialize)]
                    struct Args {
                        id: String,
                        state: Value,
                    }
                    let parsed: Args = serde_json::from_value(args)?;
                    backend.set_state(&parsed.id, parsed.state, ctx).await?;
                    Ok(ToolOutcome {
                        value: json!({ "id": parsed.id }),
                        text: format!("Set the state of \"{}\".", parsed.id),
                        details: json!({}),
                    })
                })
            },
        ));
    }

    Extension {
        name: "p2-artifacts".into(),
        tools,
        ..Default::default()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
