//! Location tools (`location_envs`, `location_dirs`, `location_move`) for session workers.

use std::sync::Arc;

use pi_rs_ai::{InputContent, Tool};
use pi_rs_chord::{context::Context, delta::Segment};
use pi_rs_durable::{
    ConversationId, DocToken, ROOT_CONVERSATION_ID,
    documents::{DocDefinition, DocumentSemantics, InitialDocument, LatestFork, resolve_address},
    harness::{Extension, ToolRegistration, configure},
    session::{Error as DurableError, Session, Transaction},
    tools::ToolResult,
};
use serde_json::{Map, Value, json};

use crate::{
    plugins::locations::{
        BoxedFuture, DirectoryListing, EnvInfo, LocationMoveState, LocationServer,
        location_move_doc,
    },
    runtime::{
        location::{location_text, parse_location, same_location},
        main_conversation::is_top_level,
        session_worker::Location,
    },
};

const MAX_LISTED: usize = 200;

pub trait LocateConversation: Send + Sync {
    fn locate<'a>(
        &'a self,
        id: ConversationId,
        session: &'a Session,
        context: &'a Context,
    ) -> BoxedFuture<'a, Location>;
}

pub fn sandbox_at<'a>(here: &Location, envs: &'a [EnvInfo]) -> Option<&'a str> {
    let env_name = here.env.as_deref()?;
    envs.iter()
        .find(|env| env.name == env_name && env.env_type == "gondolin")
        .map(|env| env.name.as_str())
}

fn describe_env(env: &EnvInfo, here: &Location) -> String {
    let target = if env.env_type == "gondolin" {
        format!("sandbox (a VM on {})", env.host)
    } else {
        let user = env
            .user
            .as_deref()
            .map(|u| format!("{u}@"))
            .unwrap_or_default();
        let port = env.port.map(|p| format!(":{p}")).unwrap_or_default();
        format!("ssh {user}{}{port}", env.host)
    };
    let tested = match &env.tested {
        None => "not tested".to_owned(),
        Some(t) if t.get("ok").and_then(Value::as_bool) == Some(true) => {
            let platform = t.get("platform").and_then(Value::as_str).unwrap_or("");
            let arch = t.get("arch").and_then(Value::as_str).unwrap_or("");
            format!("tested ok ({platform} {arch})")
        }
        Some(t) => {
            let err = t.get("error").and_then(Value::as_str).unwrap_or("");
            format!("last test failed: {err}")
        }
    };
    let here_suffix = if here.env.as_deref() == Some(env.name.as_str()) {
        " · you are here"
    } else {
        ""
    };
    format!("- {}: {target} · {tested}{here_suffix}", env.name)
}

fn text_reply(text: impl Into<String>, details: Value, is_error: bool) -> ToolResult {
    ToolResult {
        output: Some(vec![InputContent::Text(text.into().into())]),
        details: Some(details),
        is_error: Some(is_error),
        ..Default::default()
    }
}

fn subagents_doc() -> DocToken {
    Arc::new(DocDefinition {
        kind: "p2.subagents".into(),
        version: 2,
        semantics: DocumentSemantics::LatestConversation {
            fork: LatestFork::Initial,
        },
        initial: InitialDocument::Singleton(Arc::new(|| {
            Ok(Map::from_iter([
                ("agents".into(), json!({})),
                ("reporters".into(), json!({})),
            ]))
        })),
        migrate: None,
        checkpoint_when: Arc::new(|_, _, info| info.deltas_since_base >= 64),
    })
}

async fn place_subagent(
    tx: &mut Transaction<'_>,
    conversation_id: ConversationId,
    location: &Location,
) -> pi_rs_durable::session::Result<bool> {
    let doc = subagents_doc();
    let address = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)?;
    let handle = tx.doc(&doc, &address, None).await?;
    let snapshot: Value = handle
        .value()
        .map_err(|e| DurableError::invalid(e.to_string()))?
        .to_json()
        .to_serde()?;
    let Some(agents) = snapshot.get("agents").and_then(Value::as_object) else {
        return Ok(false);
    };
    let found_name = agents
        .iter()
        .find(|(_, agent)| {
            agent.get("conversationId").and_then(Value::as_u64) == Some(conversation_id)
        })
        .map(|(name, _)| name.clone());
    let Some(name) = found_name else {
        return Ok(false);
    };
    handle.set(
        &[
            Segment::Key("agents".into()),
            Segment::Key(name.into()),
            Segment::Key("location".into()),
        ],
        serde_json::to_value(location)?.into(),
    )?;
    let change = Map::from_iter([("cwd".into(), json!(location.cwd))]);
    configure(tx, conversation_id, &change).await?;
    Ok(true)
}

enum TargetResult {
    Found {
        listing: DirectoryListing,
        env: Option<String>,
    },
    Error(String),
}

async fn resolve_target(
    server: &dyn LocationServer,
    args: &Value,
    here: &Location,
    context: &Context,
) -> pi_rs_chord::Result<TargetResult> {
    let envs = server.envs(context).await?;
    let names: Vec<String> = envs.iter().map(|e| e.name.clone()).collect();
    let sandbox = sandbox_at(here, &envs);

    let location_arg = args.get("location").and_then(Value::as_str);
    let cwd_arg = args.get("cwd").and_then(Value::as_str);
    let env_arg = args.get("env");

    let parsed = if let Some(loc) = location_arg {
        parse_location(loc, &names, here.env.as_deref(), Some(&here.cwd))
    } else {
        let fallback_env: Option<&str> = match env_arg {
            None => here.env.as_deref(),
            Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => here.env.as_deref(),
        };
        let fallback_cwd = if env_arg.is_none() || fallback_env == here.env.as_deref() {
            Some(here.cwd.as_str())
        } else {
            None
        };
        parse_location(cwd_arg.unwrap_or(""), &names, fallback_env, fallback_cwd)
    };

    let parsed = match parsed {
        Ok(loc) => loc,
        Err(err) => return Ok(TargetResult::Error(err)),
    };

    if let Some(sb) = sandbox
        && parsed.env.as_deref() != Some(sb)
    {
        return Ok(TargetResult::Error(format!(
            "You work in sandbox {sb} and cannot leave it; use paths inside {sb}."
        )));
    }
    if let Some(env_name) = parsed.env.as_deref()
        && !names.iter().any(|n| n == env_name)
    {
        return Ok(TargetResult::Error(format!(
            "Unknown environment \"{env_name}\" (location_envs lists them)."
        )));
    }

    match server
        .directories(parsed.env.as_deref(), &parsed.path, context)
        .await
    {
        Ok(listing) => Ok(TargetResult::Found {
            listing,
            env: parsed.env,
        }),
        Err(err) => {
            if context.is_aborted() {
                return Err(err);
            }
            let prefix = parsed
                .env
                .as_deref()
                .map(|e| format!("{e}:"))
                .unwrap_or_default();
            Ok(TargetResult::Error(format!(
                "{prefix}{}: {err}",
                parsed.path
            )))
        }
    }
}

fn where_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "location": {
                "type": "string",
                "description": "[env:]path, e.g. gpu:~/project, local:/tmp (this device) or ../other (relative to your cwd); no prefix: your environment"
            },
            "env": {
                "anyOf": [{ "type": "string" }, { "type": "null" }],
                "description": "Instead of location: an environment, null for this device"
            },
            "cwd": {
                "type": "string",
                "description": "Instead of location: a path on env (default your environment)"
            }
        }
    })
}

async fn read_move_state(
    session: &Session,
    context: &Context,
) -> std::result::Result<LocationMoveState, DurableError> {
    let doc = location_move_doc();
    let address = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)?;
    let Some(snapshot) = session.snapshot(&doc, &address, context).await? else {
        return Ok(LocationMoveState::default());
    };
    Ok(serde_json::from_value(Value::Object((*snapshot).clone()))?)
}

pub fn create_location_extension(
    server: Arc<dyn LocationServer>,
    locate: Arc<dyn LocateConversation>,
) -> Extension {
    let envs_tool = {
        let server = server.clone();
        let locate = locate.clone();
        ToolRegistration {
            declaration: Tool {
                name: "location_envs".into(),
                description: "List the environments you can work on: this device, the configured SSH environments (host, last test) and sandboxes. Shows where you work now.".into(),
                parameters: json!({ "type": "object", "properties": {} }),
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
            execute: Arc::new(move |_args, invocation, context| {
                let server = server.clone();
                let locate = locate.clone();
                Box::pin(async move {
                    let session = &invocation.harness().session;
                    let here = locate
                        .locate(invocation.record.conversation_id, session, context)
                        .await
                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                    let envs = server
                        .envs(context)
                        .await
                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                    if let Some(sandbox) = sandbox_at(&here, &envs) {
                        let mut lines = vec![
                            format!(
                                "You work at {}, in sandbox {sandbox}: a VM whose tools reach only its own files and allowed hosts.",
                                location_text(&here)
                            ),
                            "You cannot leave it; location_dirs and location_move take paths inside it.".to_owned(),
                        ];
                        lines.extend(
                            envs.iter()
                                .filter(|e| e.name == sandbox)
                                .map(|e| describe_env(e, &here)),
                        );
                        return Ok(text_reply(lines.join("\n"), json!({}), false).into());
                    }

                    let top_level = is_top_level(session, invocation.record.conversation_id, context)
                        .await
                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                    let move_state = if top_level {
                        Some(read_move_state(session, context).await?)
                    } else {
                        None
                    };
                    let mut lines = vec![format!("You work at {}.", location_text(&here))];
                    if let Some(state) = &move_state {
                        if let Some(pending) = &state.pending {
                            lines.push(format!(
                                "Your session moves to {} when this turn ends.",
                                location_text(pending)
                            ));
                        }
                        if let Some(failed) = &state.failed {
                            lines.push(format!(
                                "The last move to {} failed: {}",
                                location_text(&failed.to),
                                failed.error
                            ));
                        }
                    }
                    lines.push("Environments:".to_owned());
                    let local_here = if here.env.is_none() {
                        " · you are here"
                    } else {
                        ""
                    };
                    lines.push(format!("- local: this device (where p2 runs){local_here}"));
                    lines.extend(envs.iter().map(|e| describe_env(e, &here)));
                    Ok(text_reply(lines.join("\n"), json!({}), false).into())
                })
            }),
        }
    };

    let dirs_tool = {
        let server = server.clone();
        let locate = locate.clone();
        ToolRegistration {
            declaration: Tool {
                name: "location_dirs".into(),
                description: "List the folders of a directory on an environment (default: your cwd), to find where to work. Shows at most 200.".into(),
                parameters: where_parameters(),
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
            execute: Arc::new(move |args, invocation, context| {
                let server = server.clone();
                let locate = locate.clone();
                Box::pin(async move {
                    let session = &invocation.harness().session;
                    let here = locate
                        .locate(invocation.record.conversation_id, session, context)
                        .await
                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                    let query = if args.get("location").is_none()
                        && args.get("cwd").is_none()
                        && args.get("env").is_none()
                    {
                        json!({ "location": here.cwd })
                    } else {
                        args
                    };
                    let found = resolve_target(&*server, &query, &here, context)
                        .await
                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                    match found {
                        TargetResult::Error(msg) => {
                            Ok(text_reply(msg, json!({}), true).into())
                        }
                        TargetResult::Found { listing, env } => {
                            let prefix = env
                                .as_deref()
                                .map(|e| format!("{e}:"))
                                .unwrap_or_default();
                            let names: Vec<String> = listing
                                .directories
                                .iter()
                                .take(MAX_LISTED)
                                .map(|p| {
                                    let seg = p.rsplit(['/', '\\']).next().unwrap_or("");
                                    format!("{seg}/")
                                })
                                .collect();
                            let more = listing.directories.len().saturating_sub(names.len());
                            let parent_part = listing
                                .parent
                                .as_deref()
                                .map(|p| format!(" (parent {prefix}{p})"))
                                .unwrap_or_default();
                            let mut lines = vec![
                                format!("{prefix}{}{parent_part}", listing.path),
                                if names.is_empty() {
                                    "No folders.".to_owned()
                                } else {
                                    names.join("\n")
                                },
                            ];
                            if more > 0 || listing.truncated {
                                let suffix = if more > 0 {
                                    format!(" ({more}+)")
                                } else {
                                    String::new()
                                };
                                lines.push(format!("More folders are not shown{suffix}."));
                            }
                            Ok(text_reply(lines.join("\n"), json!({}), false).into())
                        }
                    }
                })
            }),
        }
    };

    let move_tool = {
        ToolRegistration {
            declaration: Tool {
                name: "location_move".into(),
                description: [
                    "Move yourself to another directory or environment.",
                    "The main agent moves its whole session there once this turn has ended (after your last answer, when no subagent",
                    "work is running); subagents following the session move with it, subagents with their own location stay. Your",
                    "tools keep running where you are until then.",
                    "A subagent given this tool moves only itself, at once: its next tool calls run at the new location.",
                ]
                .join("\n"),
                parameters: where_parameters(),
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
            execute: Arc::new(move |args, invocation, context| {
                let server = server.clone();
                let locate = locate.clone();
                Box::pin(async move {
                    if args.get("location").is_none()
                        && args.get("cwd").is_none()
                        && args.get("env").is_none()
                    {
                        return Ok(text_reply(
                            "location_move needs location, or env and cwd.",
                            json!({}),
                            true,
                        )
                        .into());
                    }
                    let session = &invocation.harness().session;
                    let here = locate
                        .locate(invocation.record.conversation_id, session, context)
                        .await
                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                    let found = resolve_target(&*server, &args, &here, context)
                        .await
                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                    let (listing, env) = match found {
                        TargetResult::Error(msg) => {
                            return Ok(text_reply(msg, json!({}), true).into());
                        }
                        TargetResult::Found { listing, env } => (listing, env),
                    };
                    let to = Location {
                        env,
                        cwd: listing.path,
                    };
                    let details = json!({ "env": to.env, "cwd": to.cwd });

                    if is_top_level(session, invocation.record.conversation_id, context)
                        .await
                        .map_err(|e| DurableError::invalid(e.to_string()))?
                    {
                        let existing = read_move_state(session, context).await?;
                        if same_location(&to, &here) && existing.pending.is_none() {
                            return Ok(text_reply(
                                format!("You already work at {}.", location_text(&to)),
                                details,
                                false,
                            )
                            .into());
                        }
                        let doc = location_move_doc();
                        let address = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)?;
                        let to_val = serde_json::to_value(&to)?;
                        session
                            .commit(
                                async |tx| {
                                    let handle = tx.doc(&doc, &address, None).await?;
                                    handle.set(&[Segment::Key("pending".into())], to_val.into())?;
                                    handle.delete(&[Segment::Key("failed".into())])?;
                                    Ok(())
                                },
                                context,
                            )
                            .await?;
                        return Ok(text_reply(
                            format!(
                                "The session moves to {} when this turn ends (once no run or subagent work is active). Subagents following the session move with it; subagents with their own location stay where they are.",
                                location_text(&to)
                            ),
                            details,
                            false,
                        )
                        .into());
                    }

                    if same_location(&to, &here) {
                        return Ok(text_reply(
                            format!("You already work at {}.", location_text(&to)),
                            details,
                            false,
                        )
                        .into());
                    }

                    let conversation_id = invocation.record.conversation_id;
                    let moved = session
                        .commit(
                            async |tx| place_subagent(tx, conversation_id, &to).await,
                            context,
                        )
                        .await?;
                    if !moved {
                        return Ok(text_reply(
                            "Only the main agent and subagents can move.",
                            json!({}),
                            true,
                        )
                        .into());
                    }
                    Ok(text_reply(
                        format!(
                            "You work at {} now; the session's moves no longer move you.",
                            location_text(&to)
                        ),
                        details,
                        false,
                    )
                    .into())
                })
            }),
        }
    };

    Extension {
        name: "p2-locations".into(),
        tools: vec![envs_tool, dirs_tool, move_tool],
        ..Default::default()
    }
}
