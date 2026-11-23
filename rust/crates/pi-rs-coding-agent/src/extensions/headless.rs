//! Command adapters for the non-interactive extension host.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use indexmap::IndexMap;
use pi_rs_mcp::McpClient;
use serde_json::Value;

use crate::core::extensions::types::{
    CommandDefinition, Extension, ExtensionFactory, ExtensionInitContext, ExtensionRegistration,
    ExtensionToolContext, ToolDefinition, ToolExposure, ToolOutcome,
};

pub(crate) const COMMANDS: &[&str] = &["builtin:llama.cpp", "builtin:mcp"];

pub(crate) fn factory(
    path: &str,
    agent_dir: &Path,
    project_trusted: bool,
) -> Option<ExtensionFactory> {
    let command = match path {
        "builtin:llama.cpp" => Command::Llama,
        "builtin:mcp" => Command::Mcp {
            agent_dir: agent_dir.to_path_buf(),
            project_trusted,
        },
        _ => return None,
    };
    Some(Arc::new(move || {
        Ok(Box::new(HeadlessExtension {
            tools: IndexMap::new(),
            clients: Vec::new(),
            registration: ExtensionRegistration::new(command.path()),
            command: command.clone(),
        }))
    }))
}

#[derive(Clone)]
enum Command {
    Llama,
    Mcp {
        agent_dir: PathBuf,
        project_trusted: bool,
    },
}

impl Command {
    fn path(&self) -> &'static str {
        match self {
            Self::Llama => "builtin:llama.cpp",
            Self::Mcp { .. } => "builtin:mcp",
        }
    }
}

struct McpAgentTool {
    declaration: pi_rs_ai::Tool,
    label: String,
    client: McpClient,
    remote: String,
}

impl pi_rs_agent::AgentTool for McpAgentTool {
    fn declaration(&self) -> &pi_rs_ai::Tool {
        &self.declaration
    }
    fn label(&self) -> &str {
        &self.label
    }
    fn execute<'a>(
        &'a self,
        _id: &'a str,
        args: &'a Value,
        signal: &'a pi_rs_agent::CancellationToken,
        _update: pi_rs_agent::AgentToolUpdateCallback,
    ) -> futures_util::future::BoxFuture<'a, pi_rs_agent::Result<pi_rs_agent::AgentToolResult>>
    {
        Box::pin(async move {
            use pi_rs_agent::AgentError;
            let controller = pi_rs_mcp::AbortController::new();
            let options = pi_rs_mcp::McpRequestOptions {
                signal: Some(controller.signal()),
                ..Default::default()
            };
            let call = self
                .client
                .call_tool(&self.remote, Some(args.clone()), &options);
            tokio::pin!(call);
            let reply = tokio::select! {
                reply = &mut call => reply,
                () = signal.cancelled() => {controller.abort(None); call.await}
            }
            .map_err(|e| AgentError::Callback(e.to_string()))?;
            let blocks =
                pi_rs_mcp::to_mcp_model_content(&self.declaration.name, &reply, Default::default())
                    .await;
            let content = blocks
                .into_iter()
                .map(|block| match block {
                    pi_rs_mcp::LlmContent::Text { text } => {
                        pi_rs_ai::InputContent::Text(text.into())
                    }
                    pi_rs_mcp::LlmContent::Image { data, mime_type } => {
                        pi_rs_ai::InputContent::Image(pi_rs_ai::ImageContent {
                            kind: Default::default(),
                            data,
                            mime_type,
                        })
                    }
                })
                .collect();
            Ok(pi_rs_agent::AgentToolResult {
                content,
                structured_content: reply.structured_content,
                is_error: reply.is_error,
                ..Default::default()
            })
        })
    }
}

struct HeadlessExtension {
    tools: IndexMap<String, (McpClient, String)>,
    clients: Vec<McpClient>,
    command: Command,
    registration: ExtensionRegistration,
}

#[async_trait]
impl Extension for HeadlessExtension {
    fn agent_tools(&self) -> Vec<Arc<dyn pi_rs_agent::AgentTool>> {
        self.registration
            .tools
            .values()
            .filter(|def| matches!(def.exposure, ToolExposure::Direct | ToolExposure::ModelOnly))
            .filter_map(|def| {
                let (client, remote) = self.tools.get(&def.name)?;
                Some(Arc::new(McpAgentTool {
                    declaration: pi_rs_ai::Tool {
                        name: def.name.clone(),
                        description: def.description.clone(),
                        parameters: def.parameters.clone(),
                        constrained_sampling: None,
                    },
                    label: def.label.clone(),
                    client: client.clone(),
                    remote: remote.clone(),
                }) as Arc<dyn pi_rs_agent::AgentTool>)
            })
            .collect()
    }

    fn name(&self) -> &str {
        self.command.path()
    }
    fn replaceable(&self) -> bool {
        matches!(self.command, Command::Mcp { .. })
    }
    fn invalidate(&self) {
        for client in &self.clients {
            let client = client.clone();
            tokio::spawn(async move {
                client.close().await;
            });
        }
    }

    async fn execute_tool(
        &self,
        name: &str,
        args: Value,
        ctx: &ExtensionToolContext,
    ) -> Result<ToolOutcome, String> {
        let (client, remote) = self
            .tools
            .get(name)
            .ok_or_else(|| format!("Unknown MCP tool {name}"))?;
        let reply = client
            .call_tool(remote, Some(args.clone()), &Default::default())
            .await
            .map_err(|e| e.to_string())?;
        let failed = reply.is_error.unwrap_or(false);
        let mut outcome = ToolOutcome::success(
            serde_json::json!({"type":"toolCall","id":ctx.tool_call_id,"name":name,"arguments":args}),
            serde_json::to_value(reply).map_err(|e| e.to_string())?,
        );
        outcome.is_error = failed;
        Ok(outcome)
    }

    fn hidden(&self) -> bool {
        self.replaceable()
    }
    fn registration(&self) -> &ExtensionRegistration {
        &self.registration
    }
    fn set_registration(&mut self, registration: ExtensionRegistration) {
        self.registration = registration;
    }

    async fn init(&mut self, context: &mut ExtensionInitContext<'_>) -> Result<(), String> {
        if let Command::Mcp {
            agent_dir,
            project_trusted,
        } = &self.command
        {
            let cwd = Path::new(context.cwd);
            let loaded = crate::cli::mcp::load_config(agent_dir, cwd, *project_trusted);
            for (name, config) in loaded.servers {
                if config["enabled"] == false {
                    continue;
                }
                let Ok((client, tools)) =
                    crate::cli::mcp::discover(agent_dir, &name, &config, cwd).await
                else {
                    continue;
                };
                for tool in tools {
                    let local = pi_rs_mcp::create_mcp_tool_name(&name, &tool.name, |key| {
                        self.tools.contains_key(key)
                    });
                    let schema = pi_rs_mcp::normalize_mcp_input_schema(&tool.input_schema)
                        .map_err(|e| e.to_string())?;
                    let mut definition = ToolDefinition::new(local.clone(), schema);
                    definition.description = tool.description().unwrap_or_default().to_owned();
                    definition.label = tool.title().unwrap_or(&tool.name).to_owned();
                    definition.exposure = match config["toolExposure"]
                        .get(&tool.name)
                        .or_else(|| config.get("exposure"))
                        .and_then(Value::as_str)
                    {
                        Some("direct") => ToolExposure::Direct,
                        Some("deferred") => ToolExposure::Deferred,
                        Some("hidden") => ToolExposure::Hidden,
                        _ => ToolExposure::Codemode,
                    };
                    context.register_tool(definition)?;
                    self.tools.insert(local, (client.clone(), tool.name));
                }
                self.clients.push(client);
            }
        }
        let (name, description) = match self.command {
            Command::Llama => ("llama", "Manage llama.cpp router models"),
            Command::Mcp { .. } => (
                "mcp",
                "Manage MCP servers: sign in, reconnect, enable or disable, and change exposure",
            ),
        };
        let command = self.command.clone();
        context.register_command(CommandDefinition {
            name: name.to_owned(),
            description: Some(description.to_owned()),
            handler: Arc::new(move |args, context| {
                let command = command.clone();
                Box::pin(async move {
                    match command {
                        Command::Llama => context
                            .base
                            .ui
                            .notify("/llama is available in interactive mode", "warning"),
                        Command::Mcp {
                            agent_dir,
                            project_trusted,
                        } => {
                            let cwd = PathBuf::from(&context.base.cwd);
                            let (message, kind) =
                                mcp_command(&agent_dir, &cwd, project_trusted, &args, &context)
                                    .await;
                            context.base.ui.notify(message, kind);
                        }
                    }
                    Ok(())
                })
            }),
        })
    }
}

async fn mcp_command(
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    args: &str,
    context: &crate::core::extensions::types::ExtensionCommandContext,
) -> (String, &'static str) {
    const USAGE: &str =
        "Usage: /mcp, /mcp login [server], /mcp logout [server], /mcp reconnect [server]";
    let args = args.split_whitespace().collect::<Vec<_>>();
    if !matches!(
        args.as_slice(),
        [] | ["reconnect" | "login" | "logout"] | ["reconnect" | "login" | "logout", _]
    ) {
        return (USAGE.to_owned(), "warning");
    }
    let loaded = crate::cli::mcp::load_config(agent_dir, cwd, project_trusted);
    let servers = loaded.servers;
    let diagnostics = loaded.errors;
    let global = agent_dir.join("mcp.json");
    if let [action @ ("login" | "logout"), rest @ ..] = args.as_slice() {
        let names: Vec<_> = servers
            .iter()
            .filter(|(_, entry)| crate::cli::mcp::oauth_url(entry).is_some())
            .map(|(name, _)| name.clone())
            .collect();
        let name = match rest {
            [name] => (*name).to_owned(),
            [] if names.is_empty() => {
                return ("No MCP server uses OAuth sign-in.".into(), "warning");
            }
            [] if names.len() == 1 => names[0].clone(),
            _ => match context
                .base
                .ui
                .select_with_options("MCP server", names, Default::default())
                .await
            {
                Some(name) => name,
                None => return ("Sign-in cancelled.".into(), "info"),
            },
        };
        let Some(entry) = servers.get(&name) else {
            return (format!("No MCP server named \"{name}\"."), "warning");
        };
        let Some(url) = crate::cli::mcp::oauth_url(entry) else {
            return (
                format!("MCP server \"{name}\" does not use OAuth."),
                "warning",
            );
        };
        let result = if *action == "login" {
            crate::core::mcp_oauth::login(agent_dir, &name, url, &entry["oauth"], 300, None)
                .await
                .map(|()| format!("Signed in to MCP server \"{name}\"."))
        } else {
            crate::core::mcp_oauth::Store::new(agent_dir, &name, url)
                .and_then(|s| s.remove())
                .map(|_| format!("Signed out of MCP server \"{name}\"."))
        };
        return match result {
            Ok(message) => (message, "info"),
            Err(message) => (message, "error"),
        };
    }
    if let ["reconnect", rest @ ..] = args.as_slice() {
        if let [name] = rest
            && !servers.contains_key(*name)
        {
            return (format!("No MCP server named \"{name}\"."), "warning");
        }
        if servers.values().all(|entry| entry["enabled"] == false) {
            return ("No enabled MCP server to reconnect.".into(), "warning");
        }
        return match (context.reload)().await {
            Ok(()) => ("Reloaded MCP connections and tools.".into(), "info"),
            Err(error) => (error, "error"),
        };
    }
    if servers.is_empty() && diagnostics.is_empty() {
        return (
            format!(
                "No MCP servers configured. Add them to {} or .pi/mcp.json.",
                global.display()
            ),
            "info",
        );
    }
    let mut lines =
        futures_util::future::join_all(servers.iter().map(|(name, server)| async move {
            let exposure = server["exposure"].as_str().unwrap_or("codemode");
            if server["enabled"] == false {
                return format!("{name}: disabled ({exposure})");
            }
            match crate::cli::mcp::inspect(agent_dir, name, server, cwd).await {
                Ok(tools) => format!("{name}: connected, {} tools ({exposure})", tools.len()),
                Err(error) => format!(
                    "{name}: failed ({exposure})\n    {}",
                    error.replace('\n', "\n    ")
                ),
            }
        }))
        .await;
    lines.extend(diagnostics);
    (lines.join("\n"), "info")
}
