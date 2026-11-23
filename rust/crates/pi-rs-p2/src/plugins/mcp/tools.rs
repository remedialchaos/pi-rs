//! Durable tool registrations share names across identical MCP descriptors and check the lease at invocation.

use std::{
    borrow::Cow,
    sync::{Arc, Weak},
};

use indexmap::{IndexMap, IndexSet};
use pi_rs_ai::{ImageContent, ImageContentKind, InputContent, Tool};
use pi_rs_chord::{context::Context, host::Held};
use pi_rs_durable::{
    ConversationId,
    harness::{Extension, ToolRegistration, Wrap, section},
    session::{Error, Result},
    tool_search::{TOOL_SEARCH_TOOL_NAME, ToolSearchHost, create_tool_search_tool},
    tools::{ToolError, ToolResult},
};
use pi_rs_mcp::{
    CallToolResult, LlmContent, McpModelContentOptions, McpOutputSaver, SaveOutput,
    create_mcp_result_schema, create_mcp_tool_name, limit_mcp_content, mcp_namespace,
    mcp_server_summary, normalize_mcp_input_schema, to_mcp_model_content,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    api::v1::worker::WorkerProcess,
    runtime::{
        location::{Location, location_text, same_location},
        worker_environment::{PlacementReader, locate_conversation},
    },
};

pub(super) const NAME: &str = "mcp";

#[derive(Clone, Deserialize)]
pub(super) struct LeaseView {
    location: Location,
    instances: Vec<String>,
}

#[derive(Clone, Deserialize)]
pub(super) struct Instance {
    id: String,
    pub server: String,
    pub auth: Option<String>,
    pub description: Option<String>,
    pub instructions: Option<String>,
    pub tools: Vec<Value>,
}

pub(super) struct Offer {
    pub leases: Vec<LeaseView>,
    pub servers: Vec<Instance>,
    pub session: Location,
    pub codemode: bool,
}

#[derive(Clone)]
pub(super) struct Variant {
    server: String,
    tool: Value,
    pub name: String,
}

pub(super) type Key = (String, String, String);

pub(super) fn extension(offer: Offer, held: Held, worker: Arc<WorkerProcess>) -> Result<Extension> {
    let worker = Arc::downgrade(&worker);
    let variants = variants(&offer);
    let views = Arc::new(offer);
    let mut extension = Extension {
        name: NAME.into(),
        ..Default::default()
    };
    for variant in variants.values() {
        extension.tools.push(registration(
            variant.clone(),
            held.clone(),
            views.clone(),
            worker.clone(),
        )?);
    }
    if !views.servers.is_empty() {
        let variants = Arc::new(variants);
        if views.codemode {
            let (wrap, section) =
                super::codemode::adapt(views.clone(), worker.clone(), variants.clone());
            extension.wraps.push(wrap);
            extension.sections.push(section);
        } else {
            extension
                .sections
                .push(servers_section(views.clone(), worker.clone()));
        }
        extension.wraps.push(search_wrap(views, worker, variants));
    }
    Ok(extension)
}

fn variants(offer: &Offer) -> IndexMap<Key, Variant> {
    let mut groups: IndexMap<String, IndexSet<Key>> = IndexMap::new();
    let mut tools = IndexMap::new();
    for server in &offer.servers {
        for tool in &server.tools {
            let key = key(&server.server, tool);
            let name = create_mcp_tool_name(&key.0, &key.1, |_| false);
            groups.entry(name).or_default().insert(key.clone());
            tools.insert(key, tool.clone());
        }
    }
    let home = offer
        .servers_at(&offer.session)
        .flat_map(|server| server.tools.iter().map(|tool| key(&server.server, tool)))
        .collect::<IndexSet<_>>();
    let mut variants = IndexMap::new();
    for (plain, group) in groups {
        let distinct = group
            .iter()
            .map(|key| (&key.0, &key.1))
            .collect::<IndexSet<_>>()
            .len();
        let home_keys = group.iter().filter(|key| home.contains(*key)).count();
        for key in &group {
            let (server, name, descriptor) = key;
            let siblings = group
                .iter()
                .filter(|other| other.0 == *server && other.1 == *name)
                .count();
            let variant = if siblings > 1 && (distinct > 1 || home_keys != 1 || !home.contains(key))
            {
                let suffix = hash(format!("{server}\0{name}\0{descriptor}").as_bytes(), 8);
                let prefix: String = format!("mcp__{server}__{name}")
                    .chars()
                    .map(|ch| {
                        if ch.is_ascii_alphanumeric() || ch == '_' {
                            ch
                        } else {
                            '_'
                        }
                    })
                    .take(64 - suffix.len() - 1)
                    .collect();
                format!("{prefix}_{suffix}")
            } else if distinct > 1 {
                create_mcp_tool_name(server, name, |_| true)
            } else {
                plain.clone()
            };
            variants.insert(
                key.clone(),
                Variant {
                    server: server.clone(),
                    tool: tools[key].clone(),
                    name: variant,
                },
            );
        }
    }
    variants
}

pub(super) fn key(server: &str, tool: &Value) -> Key {
    let descriptor = json!([
        tool["title"],
        tool["annotations"]["title"],
        tool["description"],
        tool["inputSchema"],
        tool["outputSchema"]
    ]);
    (
        server.into(),
        tool["name"].as_str().unwrap_or_default().into(),
        hash(descriptor.to_string().as_bytes(), 16),
    )
}

fn hash(data: &[u8], length: usize) -> String {
    let full = format!("{:x}", Sha256::digest(data));
    full[..length].into()
}

impl Offer {
    pub(super) fn servers_at(&self, location: &Location) -> impl Iterator<Item = &Instance> {
        let lease = self
            .leases
            .iter()
            .find(|lease| same_location(&lease.location, location));
        self.servers
            .iter()
            .filter(move |server| lease.is_some_and(|lease| lease.instances.contains(&server.id)))
    }
}

fn registration(
    variant: Variant,
    held: Held,
    offer: Arc<Offer>,
    worker: Weak<WorkerProcess>,
) -> Result<ToolRegistration> {
    let tool = &variant.tool;
    let description = tool["description"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| tool["title"].as_str())
        .or_else(|| tool["annotations"]["title"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("MCP tool {} from server {}", tool["name"], variant.server));
    let parameters = normalize_mcp_input_schema(&tool["inputSchema"]).map_err(invalid)?;
    let structured_output_schema = Some(create_mcp_result_schema(tool.get("outputSchema")));
    let codemode = offer.codemode;
    Ok(ToolRegistration {
        declaration: Tool {
            name: variant.name.clone(),
            description,
            parameters,
            constrained_sampling: None,
        },
        sequential: false,
        deferred: true,
        replay_safe: false,
        callers: codemode.then(|| vec!["tools".into()]),
        prepare_arguments: None,
        cancel: None,
        output_limits: None,
        structured_output_schema,
        execute: Arc::new(move |args, invocation, ctx| {
            let (held, offer, worker, variant) =
                (held.clone(), offer.clone(), worker.clone(), variant.clone());
            Box::pin(async move {
                let session = &invocation.harness().session;
                let worker = worker
                    .upgrade()
                    .ok_or_else(|| Error::invalid("Worker stopped"))?;
                let location = locate(
                    &worker,
                    invocation.record.conversation_id,
                    Some(session),
                    ctx,
                )
                .await?;
                let expected = key(&variant.server, &variant.tool);
                let instance = offer
                    .servers_at(&location)
                    .find(|instance| instance.server == variant.server);
                if instance.is_some_and(|instance| {
                    !instance
                        .tools
                        .iter()
                        .any(|tool| key(&instance.server, tool) == expected)
                }) {
                    return Err(Error::invalid(format!(
                        "MCP tool {} is not offered at {}: find the tools there with searchTools()",
                        variant.name,
                        location_text(&location)
                    )));
                }
                let result: CallToolResult = serde_json::from_value(held.call("call",
                    json!({"location":location,"server":variant.server,"tool":expected.1,"args":args}).into(), ctx)
                    .await.map_err(invalid)?.to_serde().map_err(invalid)?)?;
                let saver = if !offer.codemode {
                    invocation
                        .env(ctx)
                        .await?
                        .map(|env| output_saver(env, ctx.clone()))
                } else {
                    None
                };
                let mut blocks = to_mcp_model_content(
                    &variant.server,
                    &result,
                    McpModelContentOptions {
                        save_output: saver.clone(),
                        ..Default::default()
                    },
                )
                .await;
                let is_error = result.is_error == Some(true);
                if is_error
                    && !blocks
                        .iter()
                        .filter_map(LlmContent::text)
                        .any(|text| !text.trim().is_empty())
                {
                    blocks.push(LlmContent::Text {
                        text: format!(
                            "MCP tool {}/{} returned an error",
                            variant.server, expected.1
                        ),
                    });
                }
                if saver.is_some() {
                    blocks = limit_mcp_content(blocks, saver).await.content;
                }
                let content = blocks
                    .into_iter()
                    .map(|block| match block {
                        LlmContent::Text { text } => InputContent::Text(text.into()),
                        LlmContent::Image { data, mime_type } => {
                            InputContent::Image(ImageContent {
                                kind: ImageContentKind::Image,
                                data,
                                mime_type,
                            })
                        }
                    })
                    .collect();
                let mut whole = serde_json::to_value(result)?;
                if let Some(fields) = whole.as_object_mut() {
                    fields.shift_remove("_meta");
                }
                Ok(ToolResult {
                    output: Some(content),
                    structured_output: Some(whole),
                    is_error: Some(is_error),
                    details: Some(json!({"server":variant.server,"tool":expected.1})),
                    ..Default::default()
                }
                .into())
            })
        }),
    })
}

fn servers_section(
    offer: Arc<Offer>,
    worker: Weak<WorkerProcess>,
) -> pi_rs_durable::harness::PromptSection {
    section(
        "mcp_servers",
        Arc::new(move |input, ctx| {
            let (offer, worker) = (offer.clone(), worker.clone());
            Box::pin(async move {
                let conversation = input["conversationId"]
                    .as_u64()
                    .ok_or_else(|| Error::invalid("Missing conversation ID"))?;
                let worker = worker
                    .upgrade()
                    .ok_or_else(|| Error::invalid("Worker stopped"))?;
                let here = locate(&worker, conversation, None, ctx).await?;
                let mut lines = Vec::new();
                for server in offer.servers_at(&here) {
                    let mut line = format!("- {} (tool_search)", mcp_namespace(&server.server));
                    let summary = mcp_server_summary(
                        server.description.as_deref(),
                        server.instructions.as_deref(),
                    );
                    let summary = if summary.chars().count() > 250 {
                        let prefix = summary.chars().take(249).collect::<String>();
                        format!("{}…", prefix.trim_end())
                    } else {
                        summary
                    };
                    if !summary.is_empty() {
                        line.push_str(&format!(": {summary}"));
                    }
                    lines.push(line);
                }
                if lines.is_empty() {
                    return Ok(None);
                }
                lines.insert(0, "MCP servers whose tools are not declared to you. Load the tools of `tool_search` servers with `tool_search`.".into());
                Ok(Some(lines.join("\n")))
            })
        }),
        None,
    )
}

struct SearchCandidates(Vec<Tool>);
impl ToolSearchHost for SearchCandidates {
    fn deferred_tools<'a, 'b, 'async_trait>(
        &'a self,
        _context: &'b Context,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = std::result::Result<Cow<'a, [Tool]>, ToolError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'a: 'async_trait,
        'b: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Ok(Cow::Borrowed(self.0.as_slice())) })
    }
}

fn search_wrap(
    offer: Arc<Offer>,
    worker: Weak<WorkerProcess>,
    variants: Arc<IndexMap<Key, Variant>>,
) -> Wrap {
    Wrap::Tool {
        name: TOOL_SEARCH_TOOL_NAME.into(),
        wrap: Arc::new(move |mut base| {
            let (offer, worker, variants) = (offer.clone(), worker.clone(), variants.clone());
            base.execute = Arc::new(move |args, invocation, ctx| {
                let (offer, worker, variants) = (offer.clone(), worker.clone(), variants.clone());
                Box::pin(async move {
                    let id = invocation.record.conversation_id;
                    let harness = invocation.harness();
                    let conversation = harness
                        .conversation(id, ctx)
                        .await?
                        .ok_or_else(|| Error::invalid("Conversation is gone"))?;
                    let worker = worker
                        .upgrade()
                        .ok_or_else(|| Error::invalid("Worker stopped"))?;
                    let here = locate(&worker, id, Some(&harness.session), ctx).await?;
                    let visible = offer
                        .servers_at(&here)
                        .flat_map(|server| {
                            server.tools.iter().filter_map(|tool| {
                                variants
                                    .get(&key(&server.server, tool))
                                    .map(|variant| variant.name.as_str())
                            })
                        })
                        .collect::<IndexSet<_>>();
                    let candidates = conversation
                        .agent(ctx)
                        .await?
                        .deferred
                        .into_iter()
                        .filter(|tool| {
                            let name = tool.declaration.name.as_str();
                            visible.contains(name)
                                || !variants.values().any(|variant| variant.name == name)
                        })
                        .map(|tool| tool.declaration)
                        .collect();
                    let result = create_tool_search_tool()
                        .execute(
                            serde_json::from_value(args)?,
                            &SearchCandidates(candidates),
                            ctx,
                        )
                        .await
                        .map_err(invalid)?;
                    Ok(ToolResult {
                        output: result.output,
                        details: result.details.map(serde_json::to_value).transpose()?,
                        control: result.control,
                        ..Default::default()
                    }
                    .into())
                })
            });
            base
        }),
    }
}

pub(super) async fn locate(
    worker: &WorkerProcess,
    id: ConversationId,
    read: Option<&dyn PlacementReader>,
    ctx: &Context,
) -> Result<Location> {
    let parts = worker.harness().map_err(invalid)?;
    locate_conversation(Some(&*parts.environments.lock().await), id, ctx, read)
        .await
        .map_err(invalid)
}

fn output_saver(env: Arc<dyn pi_rs_durable::env::ExecutionEnv>, ctx: Context) -> McpOutputSaver {
    Arc::new(move |data, ext| {
        let (env, ctx, filename) = (env.clone(), ctx.clone(), format!("output{ext}"));
        Box::pin(async move {
            let dir = env
                .create_temp_dir(Some("pi-mcp-"), &ctx)
                .await
                .map_err(|error| error.to_string())?;
            let path = env
                .join_path(&[&dir, &filename], &ctx)
                .await
                .map_err(|error| error.to_string())?;
            let data = match data {
                SaveOutput::Text(text) => text.into_bytes(),
                SaveOutput::Bytes(bytes) => bytes,
            };
            env.write_file(&path, &data, &ctx)
                .await
                .map_err(|error| error.to_string())?;
            Ok(path)
        })
    })
}

fn invalid(error: impl std::fmt::Display) -> Error {
    Error::invalid(error.to_string())
}
