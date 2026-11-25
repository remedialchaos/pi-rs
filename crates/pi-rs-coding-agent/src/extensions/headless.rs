//! Command adapters for the non-interactive extension host.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use serde_json::Value;

use crate::core::extensions::types::{
    CommandDefinition, Extension, ExtensionFactory, ExtensionInitContext, ExtensionRegistration,
    ExtensionToolContext, ToolDefinition, ToolOutcome,
};
use crate::extensions::mcp::McpConnectionManager;

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
            mcp: McpConnectionManager::new(),
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

struct HeadlessExtension {
    mcp: McpConnectionManager,
    command: Command,
    registration: ExtensionRegistration,
}

#[async_trait]
impl Extension for HeadlessExtension {
    fn agent_tools(&self) -> Vec<Arc<dyn pi_rs_agent::AgentTool>> {
        match self.command {
            Command::Mcp { .. } => self.mcp.agent_tools(),
            Command::Llama => Vec::new(),
        }
    }

    fn dynamic_tools(&self) -> Option<Vec<ToolDefinition>> {
        match self.command {
            Command::Mcp { .. } => Some(self.mcp.tool_definitions()),
            Command::Llama => None,
        }
    }

    fn name(&self) -> &str {
        self.command.path()
    }
    fn replaceable(&self) -> bool {
        matches!(self.command, Command::Mcp { .. })
    }
    fn invalidate(&self) {
        self.mcp.close_all();
    }

    async fn execute_tool(
        &self,
        name: &str,
        args: Value,
        ctx: &ExtensionToolContext,
    ) -> Result<ToolOutcome, String> {
        self.mcp.execute_tool(name, args, ctx).await
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
            self.mcp
                .connect_all(agent_dir, cwd, *project_trusted)
                .await?;
            for definition in self.mcp.tool_definitions() {
                context.register_tool(definition)?;
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
        let mcp = self.mcp.clone();
        context.register_command(CommandDefinition {
            name: name.to_owned(),
            description: Some(description.to_owned()),
            handler: Arc::new(move |args, context| {
                let command = command.clone();
                let mcp = mcp.clone();
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
                            let (message, kind) = mcp_command(
                                &agent_dir,
                                &cwd,
                                project_trusted,
                                &args,
                                &context,
                                &mcp,
                            )
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
    mcp: &McpConnectionManager,
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
        if let [name] = rest {
            let Some(entry) = servers.get(*name) else {
                return (format!("No MCP server named \"{name}\"."), "warning");
            };
            if entry["enabled"] == false {
                mcp.disconnect_server(name).await;
                context.base.host.refresh_tools();
                return (format!("MCP server \"{name}\" is disabled."), "warning");
            }
            let result = mcp
                .reconnect_server(name, agent_dir, cwd, project_trusted)
                .await;
            context.base.host.refresh_tools();
            return match result {
                Ok(outcome) => (
                    format!(
                        "Reconnected MCP server \"{name}\" ({} tools).",
                        outcome.tool_count
                    ),
                    "info",
                ),
                Err(error) => (
                    format!("Failed to reconnect MCP server \"{name}\": {error}"),
                    "error",
                ),
            };
        }
        if servers.values().all(|entry| entry["enabled"] == false) {
            return ("No enabled MCP server to reconnect.".into(), "warning");
        }
        let result = mcp.reconnect_all(agent_dir, cwd, project_trusted).await;
        context.base.host.refresh_tools();
        return match result {
            Ok(_) => ("Reloaded MCP connections and tools.".into(), "info"),
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
