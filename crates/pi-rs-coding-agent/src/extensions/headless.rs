//! Built-in extension adapters for the native extension host.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::core::event_bus::ExtensionEvent;
use crate::core::extensions::types::{
    CommandDefinition, Extension, ExtensionFactory, ExtensionInitContext, ExtensionRegistration,
    ExtensionToolContext, ToolDefinition, ToolOutcome, sync_handler,
};
use crate::extensions::mcp::{MCP_SERVERS_SECTION, McpConnectionManager};

pub(crate) const COMMANDS: &[&str] = &[
    "builtin:llama.cpp",
    "builtin:codemode",
    "builtin:tool-search",
    "builtin:mcp",
];

pub(crate) fn factory(
    path: &str,
    agent_dir: &Path,
    project_trusted: bool,
) -> Option<ExtensionFactory> {
    let command = match path {
        "builtin:llama.cpp" => Command::Llama {
            agent_dir: agent_dir.to_path_buf(),
        },
        "builtin:codemode" => Command::Codemode,
        "builtin:tool-search" => Command::ToolSearch,
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
    Llama {
        agent_dir: PathBuf,
    },
    Codemode,
    ToolSearch,
    Mcp {
        agent_dir: PathBuf,
        project_trusted: bool,
    },
}

impl Command {
    fn path(&self) -> &'static str {
        match self {
            Self::Llama { .. } => "builtin:llama.cpp",
            Self::Codemode => "builtin:codemode",
            Self::ToolSearch => "builtin:tool-search",
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
            Command::Llama { .. } | Command::Codemode | Command::ToolSearch => Vec::new(),
        }
    }

    fn dynamic_tools(&self) -> Option<Vec<ToolDefinition>> {
        match self.command {
            Command::Mcp { .. } => Some(self.mcp.tool_definitions()),
            Command::Llama { .. } | Command::Codemode | Command::ToolSearch => None,
        }
    }

    fn name(&self) -> &str {
        self.command.path()
    }
    fn replaceable(&self) -> bool {
        !matches!(self.command, Command::Llama { .. })
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
        match self.command {
            Command::Codemode if name == crate::extensions::codemode::CODEMODE_TOOL_NAME => {
                crate::extensions::codemode::execute_codemode(
                    args,
                    ctx,
                    Default::default(),
                    Some(&ctx.base.host),
                )
                .await
            }
            Command::ToolSearch
                if name == crate::extensions::tool_search::TOOL_SEARCH_TOOL_NAME =>
            {
                crate::extensions::tool_search::execute_tool_search(args, ctx).await
            }
            Command::Mcp { .. } => {
                if let Some(res) = self.mcp.execute_resource_tool(name, &args, ctx).await {
                    res
                } else {
                    self.mcp.execute_tool(name, args, ctx).await
                }
            }
            _ => Err(format!("Unknown built-in extension tool {name}")),
        }
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
        match &self.command {
            Command::Llama { agent_dir } => {
                context.register_provider(crate::extensions::llama::provider_registration(
                    context.registration.path.as_str(),
                ))?;
                let agent_dir = agent_dir.clone();
                context.register_command(CommandDefinition {
                    name: "llama".to_owned(),
                    description: Some("Manage llama.cpp router models".to_owned()),
                    handler: Arc::new(move |args, command_ctx| {
                        let agent_dir = agent_dir.clone();
                        Box::pin(async move {
                            crate::extensions::llama::run_llama_command(
                                &agent_dir,
                                &args,
                                &command_ctx,
                            )
                            .await;
                            Ok(())
                        })
                    }),
                })
            }
            Command::Codemode => {
                context.register_tool(crate::extensions::codemode::codemode_tool_definition())
            }
            Command::ToolSearch => {
                context.register_tool(crate::extensions::tool_search::tool_search_definition())
            }
            Command::Mcp {
                agent_dir,
                project_trusted,
            } => {
                let cwd = Path::new(context.cwd);
                self.mcp
                    .connect_all(agent_dir, cwd, *project_trusted)
                    .await?;
                for definition in self.mcp.tool_definitions() {
                    context.register_tool(definition)?;
                }
                for definition in crate::extensions::mcp::mcp_resource_tool_definitions() {
                    context.register_tool(definition)?;
                }
                let mcp_for_prompt = self.mcp.clone();
                let agent_dir_prompt = agent_dir.clone();
                let cwd_prompt = cwd.to_path_buf();
                let trusted_prompt = *project_trusted;
                context.on(
                    ExtensionEvent::BeforeAgentStart,
                    sync_handler(move |_event| {
                        Ok(mcp_for_prompt
                            .servers_section(&agent_dir_prompt, &cwd_prompt, trusted_prompt)
                            .map(|section| {
                                json!({
                                    "sections": {
                                        MCP_SERVERS_SECTION: section,
                                    }
                                })
                            }))
                    }),
                )?;
                let agent_dir = agent_dir.clone();
                let project_trusted = *project_trusted;
                let mcp = self.mcp.clone();
                context.register_command(CommandDefinition {
                    name: "mcp".to_owned(),
                    description: Some(
                        "Manage MCP servers: sign in, reconnect, enable or disable, and change exposure"
                            .to_owned(),
                    ),
                    handler: Arc::new(move |args, command_ctx| {
                        let agent_dir = agent_dir.clone();
                        let mcp = mcp.clone();
                        Box::pin(async move {
                            let cwd = PathBuf::from(&command_ctx.base.cwd);
                            if args.trim().is_empty() && command_ctx.base.ui.has_ui {
                                mcp_interactive_dialog(
                                    &agent_dir,
                                    &cwd,
                                    project_trusted,
                                    &command_ctx,
                                    &mcp,
                                )
                                .await;
                                return Ok(());
                            }
                            let (message, kind) = mcp_command(
                                &agent_dir,
                                &cwd,
                                project_trusted,
                                &args,
                                &command_ctx,
                                &mcp,
                            )
                            .await;
                            command_ctx.base.ui.notify(message, kind);
                            Ok(())
                        })
                    }),
                })
            }
        }
    }
}

async fn mcp_interactive_dialog(
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    context: &crate::core::extensions::types::ExtensionCommandContext,
    mcp: &McpConnectionManager,
) {
    let loaded = crate::cli::mcp::load_config(agent_dir, cwd, project_trusted);
    if loaded.servers.is_empty() {
        let global = agent_dir.join("mcp.json");
        context.base.ui.notify(
            format!(
                "No MCP servers configured. Add them to {} or .pi/mcp.json.",
                global.display()
            ),
            "info",
        );
        return;
    }
    let connected = mcp.server_names();
    let server_items: Vec<String> = loaded
        .servers
        .iter()
        .map(|(name, entry)| {
            let exposure = entry["exposure"].as_str().unwrap_or("codemode");
            let state = if entry["enabled"] == false {
                "disabled"
            } else if connected.iter().any(|s| s == name) {
                "connected"
            } else {
                "disconnected"
            };
            format!("{name} ({state}, {exposure})")
        })
        .collect();
    let Some(picked) = context
        .base
        .ui
        .select_with_options("MCP servers", server_items, Default::default())
        .await
    else {
        return;
    };
    let server_name = picked.split(" (").next().unwrap_or(&picked).to_owned();
    let Some(entry) = loaded.servers.get(&server_name) else {
        return;
    };
    let enabled = entry["enabled"] != false;
    let mut actions = vec!["Reconnect".to_owned()];
    if crate::cli::mcp::oauth_url(entry).is_some() {
        actions.push("Sign in (OAuth)".to_owned());
        actions.push("Sign out (OAuth)".to_owned());
    }
    actions.push(if enabled {
        "Disable".to_owned()
    } else {
        "Enable".to_owned()
    });
    actions.push("Change exposure".to_owned());
    let Some(action) = context
        .base
        .ui
        .select_with_options(
            format!("MCP server: {server_name}"),
            actions,
            Default::default(),
        )
        .await
    else {
        return;
    };
    match action.as_str() {
        "Reconnect" => {
            let (msg, kind) = mcp_command(
                agent_dir,
                cwd,
                project_trusted,
                &format!("reconnect {server_name}"),
                context,
                mcp,
            )
            .await;
            context.base.ui.notify(msg, kind);
        }
        "Sign in (OAuth)" => {
            let (msg, kind) = mcp_command(
                agent_dir,
                cwd,
                project_trusted,
                &format!("login {server_name}"),
                context,
                mcp,
            )
            .await;
            context.base.ui.notify(msg, kind);
        }
        "Sign out (OAuth)" => {
            let (msg, kind) = mcp_command(
                agent_dir,
                cwd,
                project_trusted,
                &format!("logout {server_name}"),
                context,
                mcp,
            )
            .await;
            context.base.ui.notify(msg, kind);
        }
        "Disable" => {
            if let Err(err) = crate::extensions::mcp::update_mcp_server_config_entry(
                agent_dir,
                cwd,
                project_trusted,
                &server_name,
                Some(false),
                None,
            ) {
                context.base.ui.notify(err, "error");
            } else {
                mcp.disconnect_server(&server_name).await;
                context.base.host.refresh_tools();
                context
                    .base
                    .ui
                    .notify(format!("Disabled MCP server \"{server_name}\"."), "info");
            }
        }
        "Enable" => {
            if let Err(err) = crate::extensions::mcp::update_mcp_server_config_entry(
                agent_dir,
                cwd,
                project_trusted,
                &server_name,
                Some(true),
                None,
            ) {
                context.base.ui.notify(err, "error");
            } else {
                let (msg, kind) = mcp_command(
                    agent_dir,
                    cwd,
                    project_trusted,
                    &format!("reconnect {server_name}"),
                    context,
                    mcp,
                )
                .await;
                context.base.ui.notify(msg, kind);
            }
        }
        "Change exposure" => {
            let modes = vec![
                "codemode".to_owned(),
                "deferred".to_owned(),
                "direct".to_owned(),
                "hidden".to_owned(),
            ];
            if let Some(exp) = context
                .base
                .ui
                .select_with_options(
                    format!("Exposure for {server_name}"),
                    modes,
                    Default::default(),
                )
                .await
            {
                if let Err(err) = crate::extensions::mcp::update_mcp_server_config_entry(
                    agent_dir,
                    cwd,
                    project_trusted,
                    &server_name,
                    None,
                    Some(&exp),
                ) {
                    context.base.ui.notify(err, "error");
                } else {
                    let _ = mcp
                        .reconnect_server(&server_name, agent_dir, cwd, project_trusted)
                        .await;
                    context.base.host.refresh_tools();
                    context.base.ui.notify(
                        format!("Updated exposure for \"{server_name}\" to {exp}."),
                        "info",
                    );
                }
            }
        }
        _ => {}
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
