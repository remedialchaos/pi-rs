use std::sync::Arc;

use indexmap::IndexMap;
use pi_rs_ai::{InputContent, Tool, registry::Models};
use pi_rs_chord::{
    Result,
    context::Context,
    contracts::Contract,
    delta::Segment,
    host::{Members, ServeOptions, define_facet},
};
use pi_rs_durable::{
    ASSISTANT_ENTRY, ConversationId, ConversationOwnership, DocDefinition, DocToken,
    documents::{DocumentSemantics, InitialDocument, resolve_address},
    harness::{
        Extension, InputEntry, Invocation, SubmissionBody, SubmissionDraft, ToolExecution,
        ToolRegistration, configure,
    },
    session::{Error as DurableError, Result as DurableResult},
    storage_types::{SubmissionKind, SubmissionStatus},
    tools::ToolResult,
};
use pi_rs_schema::{CompileOptions, Validator, compile_with, formats::formats};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    api::v1::{FacetKind, PluginRegistry, worker::worker_of},
    runtime::main_conversation::agent_conversation,
};

use super::subagents::{
    contract::SUBAGENT_TASK_ENTRY,
    contract_worker::{
        AgentChoice, AgentKind, OneShot, SubagentState, ToolScope, inherited_location_doc,
        resolve_choice, subagents_doc, thinking_level_str,
    },
    tools::own_location,
};

pub const AGENT_TOOL: &str = "agent";
const ATTEMPTS: usize = 3;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Arguments {
    prompt: String,
    label: Option<String>,
    schema: Map<String, Value>,
    model: Option<String>,
    thinking_level: Option<String>,
    tools: Option<Vec<String>>,
}

fn run_doc() -> DocToken {
    Arc::new(DocDefinition {
        kind: "p2.agent-run".into(),
        version: 1,
        semantics: DocumentSemantics::Task,
        initial: InitialDocument::Singleton(Arc::new(|| Ok(Map::new()))),
        migrate: None,
        checkpoint_when: Arc::new(|_, _, _| true),
    })
}

fn failure(text: impl Into<String>) -> ToolExecution {
    ToolResult {
        output: Some(vec![InputContent::Text(text.into().into())]),
        is_error: Some(true),
        ..Default::default()
    }
    .into()
}

fn answer_value(validator: &Validator, text: &str) -> std::result::Result<Value, String> {
    let trimmed = text.trim();
    let unfenced = trimmed.strip_prefix("```").and_then(|body| {
        let (language, rest) = body.split_once('\n')?;
        matches!(language.trim(), "" | "json").then_some(rest.strip_suffix("```")?)
    });
    let value: Value = serde_json::from_str(unfenced.unwrap_or(text))
        .map_err(|error| format!("it is not JSON ({error})"))?;
    let candidate = (&value).into();
    if validator.check(&candidate) {
        return Ok(value);
    }
    let errors = validator
        .errors(&candidate)
        .into_iter()
        .take(5)
        .map(|error| {
            format!(
                "{}: {}",
                if error.path.is_empty() {
                    "/"
                } else {
                    &error.path
                },
                error.message
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!("it does not match the schema ({errors})"))
}

async fn execute(
    raw_args: Value,
    inv: &Invocation,
    ctx: &Context,
    models: &Models,
) -> DurableResult<ToolExecution> {
    let args: Arguments = serde_json::from_value(raw_args)?;
    let schema = Value::Object(args.schema);
    let validator = match compile_with(&schema, &CompileOptions::new().with_formats(formats())) {
        Ok(validator) => validator,
        Err(error) => return Ok(failure(format!("Invalid schema: {error}"))),
    };
    let harness = inv.harness();
    let caller = inv.record.conversation_id;
    let task_id = inv.record.id;
    let call_id = inv.record.input["callId"]
        .as_str()
        .or_else(|| inv.record.input["call"]["id"].as_str())
        .ok_or_else(|| DurableError::invalid("Missing agent call ID"))?;
    let run = run_doc();
    let run_addr = resolve_address(&run, Some(task_id), None)?;
    let existing = harness
        .session
        .snapshot(&run, &run_addr, ctx)
        .await?
        .and_then(|value| value.get("conversationId").and_then(Value::as_u64));
    let choice = if existing.is_none() {
        let caller = harness
            .conversation(caller, ctx)
            .await?
            .ok_or_else(|| DurableError::invalid("Caller conversation missing"))?;
        let access = IndexMap::from([
            ("subagent".into(), ToolScope::Main),
            ("team".into(), ToolScope::Subagent),
        ]);
        match resolve_choice(
            models,
            &caller.agent(ctx).await?,
            &access,
            &AgentChoice {
                model: args.model,
                thinking_level: args.thinking_level,
                tools: args.tools,
            },
            None,
            AgentKind::OneShot,
            ctx,
        )
        .await
        {
            Ok(choice) => Some(choice),
            Err(error) => return Ok(failure(error)),
        }
    } else {
        None
    };
    let inherited = own_location(&harness.session, caller, ctx)
        .await
        .map_err(|error| DurableError::invalid(error.to_string()))?;
    let listed_on = agent_conversation(&harness.session, caller, ctx)
        .await
        .map_err(|error| DurableError::invalid(error.to_string()))?;
    let id: ConversationId = harness.commit(async |tx| {
        let state: Value = tx.doc(&run, &run_addr, None).await?.value()?.to_json().to_serde()?;
        if let Some(id) = state["conversationId"].as_u64() {
            return Ok(id);
        }
        let choice = choice.ok_or_else(|| DurableError::invalid("The agent's conversation record disappeared"))?;
        let child = tx.create_conversation(ConversationOwnership::Task { task_id }).await?.id;
        let thinking = thinking_level_str(choice.thinking_level);
        let mut patch = Map::from_iter([
            ("model".into(), serde_json::to_value(&choice.model)?),
            ("thinkingLevel".into(), json!(thinking)),
            ("tools".into(), json!(choice.tools.tools.iter().map(|tool| &tool.declaration.name).collect::<Vec<_>>())),
            ("modelTools".into(), json!({"add": choice.tools.loaded.iter().map(|tool| &tool.declaration.name).collect::<Vec<_>>()})),
            ("instructions".into(), json!(format!(
                "You are a one-shot subagent working for a script. Do the requested work. Your final answer is parsed by the script: it must be only a JSON value matching this JSON Schema, without prose or code fences:\n{schema}"
            ))),
        ]);
        if let Some(location) = inherited {
            patch.insert("cwd".into(), json!(location.cwd));
            let doc = inherited_location_doc();
            let address = resolve_address(&doc, Some(child), None)?;
            tx.doc(&doc, &address, None).await?
                .set(&[Segment::Key("location".into())], serde_json::to_value(location)?.into())?;
        }
        configure(tx, child, &patch).await?;
        tx.doc(&run, &run_addr, None).await?
            .set(&[Segment::Key("conversationId".into())], json!(child).into())?;
        let doc = subagents_doc();
        let address = resolve_address(&doc, Some(listed_on), None)?;
        let registry = tx.doc(&doc, &address, None).await?;
        let mut state: SubagentState = serde_json::from_value(registry.value()?.to_json().to_serde()?)?;
        let shots = state.one_shots.get_or_insert_with(IndexMap::new);
        shots.insert(call_id.into(), OneShot {
            conversation_id: child,
            label: args.label.as_deref().unwrap_or(&args.prompt).split_whitespace().collect::<Vec<_>>()
                .join(" ").chars().scan(0, |units, ch| { *units += ch.len_utf16(); (*units <= 80).then_some(ch) }).collect(),
            model: format!("{}/{}", choice.model.provider, choice.model.model_id),
            thinking_level: thinking.into(),
            tools: choice.tools.chosen,
        });
        registry.set(&[Segment::Key("oneShots".into())], serde_json::to_value(shots)?.into())?;
        Ok(child)
    }, ctx).await?;
    let Some(child) = harness.conversation(id, ctx).await? else {
        return Ok(failure("The agent's conversation is gone."));
    };
    let mut message = args.prompt;
    let mut problem = String::new();
    for attempt in 1..=ATTEMPTS {
        let settled = child
            .submit(
                SubmissionDraft {
                    request_id: Some(format!("agent:{attempt}")),
                    notify: None,
                    body: SubmissionBody::Input {
                        content: json!(message),
                        entry: Some(InputEntry {
                            kind: SUBAGENT_TASK_ENTRY.into(),
                            data: Some(json!({})),
                        }),
                        when_busy: Some("followUp".into()),
                        when_idle: None,
                        on_abort: None,
                    },
                },
                ctx,
            )
            .await?
            .wait(ctx)
            .await?;
        if settled.kind != SubmissionKind::Input || settled.status != SubmissionStatus::Done {
            return Ok(failure(format!(
                "The agent did not answer: {}",
                settled.reason.as_deref().unwrap_or(settled.status.as_str())
            )));
        }
        let mut text = String::new();
        if let Some(answer) = settled.answer
            && let Some(stored) = harness.session.entry(answer, ctx).await?
            && ASSISTANT_ENTRY.matches(Some(&stored.entry))
            && let Some(parts) = stored
                .entry
                .model
                .as_ref()
                .and_then(|model| model.first())
                .and_then(|answer| answer["content"].as_array())
        {
            for part in parts.iter().filter(|part| part["type"] == "text") {
                if let Some(fragment) = part["text"].as_str() {
                    text.push_str(fragment);
                }
            }
        }
        match answer_value(&validator, &text) {
            Ok(value) => {
                return Ok(ToolResult {
                    output: Some(vec![InputContent::Text(value.to_string().into())]),
                    structured_output: Some(value),
                    details: Some(json!({"conversationId": id, "attempts": attempt})),
                    ..Default::default()
                }
                .into());
            }
            Err(error) => {
                let correction = if error.starts_with("it is not JSON") {
                    ""
                } else {
                    "corrected "
                };
                message = format!(
                    "Your answer was not usable: {error}. Reply with only the {correction}JSON value."
                );
                problem = error;
            }
        }
    }
    Ok(failure(format!(
        "The agent's answer was not usable after {ATTEMPTS} attempts: {problem}."
    )))
}

pub fn extension(models: Models) -> Extension {
    Extension {
        name: "p2-agent".into(),
        tools: vec![ToolRegistration {
            declaration: Tool {
                name: AGENT_TOOL.into(),
                description: "Run a one-shot subagent in your cwd and resolve to its final answer, parsed as JSON and validated against `schema` (a JSON Schema object). Rejects when the subagent fails, or when its answer still does not validate after 3 attempts. Its answer is not reported to you, only what the script returns. Run many at once with `Promise.all`.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "prompt": {"type": "string", "description": "The task."},
                        "label": {"type": "string", "description": "Short name shown where the user inspects it; default: the prompt's start."},
                        "schema": {"type": "object", "patternProperties": {"^.*$": {}}, "description": "JSON Schema of the answer."},
                        "model": {"type": "string", "description": "'provider/modelId'; default: the caller's."},
                        "thinkingLevel": {"type": "string", "description": "off|minimal|low|medium|high|xhigh|max; default: the caller's."},
                        "tools": {"type": "array", "items": {"type": "string"}, "description": "Names of tools of yours it gets (default: all you can give); not those that work only in your conversation."}
                    },
                    "required": ["prompt", "schema"], "additionalProperties": true
                }),
                constrained_sampling: None,
            },
            sequential: false,
            prepare_arguments: None,
            cancel: None,
            structured_output_schema: Some(json!({})),
            output_limits: None,
            replay_safe: true,
            callers: Some(vec!["tools".into()]),
            deferred: false,
            execute: Arc::new(move |args, invocation, context| {
                let models = models.clone();
                Box::pin(async move { execute(args, invocation, context, &models).await })
            }),
        }],
        ..Default::default()
    }
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let contract = Arc::new(Contract::local(
        "p2.agent-tool",
        1,
        Vec::new(),
        IndexMap::new(),
    )?);
    registry.register(
        "agent",
        FacetKind::Worker,
        define_facet(move |host| {
            host.serve(
                contract.clone(),
                |_, _, object| {
                    let worker = worker_of(&object.host())?;
                    let registries = worker.registries.clone();
                    let process = Arc::downgrade(&worker);
                    registries
                        .read_only
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .insert(
                            "agent".into(),
                            Arc::new(move |id, context| {
                                let process = process.clone();
                                Box::pin(async move {
                                    let process =
                                        process.upgrade().ok_or(pi_rs_chord::Error::Closed)?;
                                    let parts = process.harness()?;
                                    let session = &parts.durable.session;
                                    let Some(owner) = session
                                        .conversation(id, &context)
                                        .await
                                        .map_err(durable_error)?
                                        .and_then(|conversation| conversation.owner)
                                    else {
                                        return Ok(None);
                                    };
                                    let listed_on = agent_conversation(
                                        session,
                                        owner.conversation_id,
                                        &context,
                                    )
                                    .await
                                    .map_err(durable_error)?;
                                    let doc = subagents_doc();
                                    let address = resolve_address(&doc, Some(listed_on), None)
                                        .map_err(durable_error)?;
                                    let Some(snapshot) = session
                                        .snapshot(&doc, &address, &context)
                                        .await
                                        .map_err(durable_error)?
                                    else {
                                        return Ok(None);
                                    };
                                    let state: SubagentState =
                                        serde_json::from_value(Value::Object((*snapshot).clone()))?;
                                    Ok(state
                                        .one_shots
                                        .is_some_and(|shots| {
                                            shots.values().any(|shot| shot.conversation_id == id)
                                        })
                                        .then(|| "One-shot agents are read-only".into()))
                                })
                            }),
                        );
                    let extension = extension(registries.models.clone());
                    let name = extension.name.clone();
                    registries.extensions.install(extension)?;
                    object.open(
                        |_| Box::pin(async { Ok(()) }),
                        move |_, _| {
                            let registries = registries.clone();
                            let name = name.clone();
                            Box::pin(async move {
                                registries
                                    .read_only
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .shift_remove("agent");
                                registries.extensions.uninstall(&name)
                            })
                        },
                    );
                    Ok(Members::new())
                },
                ServeOptions::default(),
            )
        }),
    )
}

fn durable_error(error: impl std::fmt::Display) -> pi_rs_chord::Error {
    pi_rs_chord::Error::Invalid(error.to_string())
}
