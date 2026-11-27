//! The built-in MCP extension adapter.
//!
//! Upstream (`packages/coding-agent/src/extensions/mcp/`) connects servers from `mcp.json`,
//! registers their tools as `mcp__<server>__<tool>`, and exposes a `/mcp` manager. Transport and
//! OAuth live in `pi_rs_mcp`; this module owns the coding-agent-facing contract: server config,
//! exposure resolution, tool naming, and the `mcp_servers` system-prompt section.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use pi_rs_mcp::{McpClient, create_mcp_tool_name, mcp_namespace, mcp_server_summary};
use serde_json::Value;

use crate::core::extensions::types::{
    ExtensionToolContext, ToolDefinition, ToolExposure as RuntimeToolExposure, ToolOutcome,
};

use super::{Extension, ExtensionRegistry, ToolExposure, ToolRegistration};

pub const MCP_SERVERS_SECTION: &str = "mcp_servers";
pub const MAX_SERVER_DESCRIPTION_CHARS: usize = 250;
pub const MAX_SERVERS_SECTION_CHARS: usize = 4096;
pub const DEFAULT_STARTUP_WAIT_MS: u64 = 10_000;

pub const LIST_MCP_RESOURCES_TOOL: &str = "list_mcp_resources";
pub const LIST_MCP_RESOURCE_TEMPLATES_TOOL: &str = "list_mcp_resource_templates";
pub const READ_MCP_RESOURCE_TOOL: &str = "read_mcp_resource";

/// How a server's tools are made reachable, matching upstream `McpExposure`.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum McpExposure {
    #[default]
    Codemode,
    Deferred,
    Direct,
    Hidden,
}

impl McpExposure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codemode => "codemode",
            Self::Deferred => "deferred",
            Self::Direct => "direct",
            Self::Hidden => "hidden",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "codemode" => Some(Self::Codemode),
            "deferred" => Some(Self::Deferred),
            "direct" => Some(Self::Direct),
            "hidden" => Some(Self::Hidden),
            _ => None,
        }
    }

    pub fn tool_exposure(self) -> ToolExposure {
        match self {
            Self::Codemode => ToolExposure::Codemode,
            Self::Deferred => ToolExposure::Deferred,
            Self::Direct => ToolExposure::Model,
            Self::Hidden => ToolExposure::Hidden,
        }
    }

    pub fn is_indirect(self) -> bool {
        matches!(self, Self::Codemode | Self::Deferred)
    }
}

/// A server entry from `mcp.json` or an extension registration.
#[derive(Clone, Debug)]
pub struct McpServerConfig {
    pub name: String,
    pub description: Option<String>,
    pub instructions: Option<String>,
    pub exposure: McpExposure,
    pub tool_exposure: BTreeMap<String, McpExposure>,
    pub enabled: bool,
}

impl McpServerConfig {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            instructions: None,
            exposure: McpExposure::Codemode,
            tool_exposure: BTreeMap::new(),
            enabled: true,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    pub fn with_exposure(mut self, exposure: McpExposure) -> Self {
        self.exposure = exposure;
        self
    }

    pub fn with_tool_exposure(mut self, tool: impl Into<String>, exposure: McpExposure) -> Self {
        self.tool_exposure.insert(tool.into(), exposure);
        self
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    pub fn exposure_for_tool(&self, tool: &str) -> McpExposure {
        self.tool_exposure
            .get(tool)
            .copied()
            .unwrap_or(self.exposure)
    }

    pub fn configured_exposures(&self) -> BTreeSet<McpExposure> {
        let mut exposures = BTreeSet::new();
        exposures.insert(self.exposure);
        exposures.extend(self.tool_exposure.values().copied());
        exposures
    }

    pub fn has_direct_tools(&self) -> bool {
        self.configured_exposures().contains(&McpExposure::Direct)
    }

    pub fn has_indirect_tools(&self) -> bool {
        self.configured_exposures()
            .iter()
            .any(|exposure| exposure.is_indirect())
    }

    pub fn namespace(&self) -> String {
        mcp_namespace(&self.name)
    }

    pub fn tool_name(&self, tool: &str, is_taken: impl Fn(&str) -> bool) -> String {
        create_mcp_tool_name(&self.name, tool, is_taken)
    }
}

/// What a server contributes to the `mcp_servers` system-prompt section.
#[derive(Clone, Debug)]
pub struct McpServerListing {
    pub config: McpServerConfig,
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max <= 1 {
        return String::new();
    }
    let prefix: String = text.chars().take(max - 1).collect();
    format!("{}…", prefix.trim_end())
}

fn section_intro(reaches: &BTreeSet<&str>) -> String {
    let mut intro = "MCP servers whose tools are not declared to you.".to_string();
    if reaches.contains("codemode") {
        intro.push_str(" Call the tools of `codemode` servers from codemode scripts.");
    }
    if reaches.contains("tool_search") {
        intro.push_str(" Load the tools of `tool_search` servers with `tool_search`.");
    }
    intro
}

/// Render the `mcp_servers` section: one line per enabled server with indirect tools, with the
/// reach mechanism and a one-line summary. Returns `None` when no server needs listing.
pub fn render_servers_section(servers: &[McpServerListing]) -> Option<String> {
    let mut listed: Vec<&McpServerConfig> = servers
        .iter()
        .map(|listing| &listing.config)
        .filter(|config| config.enabled && config.has_indirect_tools())
        .collect();
    if listed.is_empty() {
        return None;
    }
    listed.sort_by(|left, right| left.name.cmp(&right.name));
    let reaches: BTreeSet<&str> = listed
        .iter()
        .map(|config| {
            if config
                .configured_exposures()
                .contains(&McpExposure::Codemode)
            {
                "codemode"
            } else {
                "tool_search"
            }
        })
        .collect();
    let intro = section_intro(&reaches);
    let heads: Vec<String> = listed
        .iter()
        .map(|config| {
            let reach = if config
                .configured_exposures()
                .contains(&McpExposure::Codemode)
            {
                "codemode"
            } else {
                "tool_search"
            };
            format!("- {} ({reach})", config.namespace())
        })
        .collect();
    let omitted = |count: usize| -> Vec<String> {
        if count == 0 {
            Vec::new()
        } else {
            vec![format!(
                "- … {count} more server{}; find their tools with searchTools()",
                if count == 1 { "" } else { "s" }
            )]
        }
    };
    let size = |kept: usize| {
        [
            intro.clone(),
            heads[..kept].join("\n"),
            omitted(listed.len() - kept).join("\n"),
        ]
        .join("\n")
        .len()
    };
    let mut kept = listed.len();
    while kept > 0 && size(kept) > MAX_SERVERS_SECTION_CHARS {
        kept -= 1;
    }
    let per_server = MAX_SERVER_DESCRIPTION_CHARS.min(
        MAX_SERVERS_SECTION_CHARS
            .saturating_sub(size(kept))
            .checked_div(kept)
            .map_or(0, |quotient| quotient.saturating_sub(2)),
    );
    let mut lines = Vec::new();
    for (index, config) in listed.iter().take(kept).enumerate() {
        let summary = if per_server > 0 {
            truncate(
                &mcp_server_summary(
                    config.description.as_deref(),
                    config.instructions.as_deref(),
                ),
                per_server,
            )
        } else {
            String::new()
        };
        lines.push(if summary.is_empty() {
            heads[index].clone()
        } else {
            format!("{}: {}", heads[index], summary)
        });
    }
    lines.extend(omitted(listed.len() - kept));
    Some([intro, lines.join("\n")].join("\n"))
}

/// Whether a call that reaches `reach` needs to wait for `server`.
pub fn reaches_server(reach: &[String], server: &str) -> bool {
    let namespace = mcp_namespace(server);
    reach.iter().any(|word| word.contains(&namespace))
}

#[derive(Clone, Copy, Debug, Default)]
pub struct McpExtensionOptions {
    pub startup_wait_ms: Option<u64>,
}

impl McpExtensionOptions {
    pub fn startup_wait_ms(&self) -> u64 {
        self.startup_wait_ms.unwrap_or(DEFAULT_STARTUP_WAIT_MS)
    }
}

/// The built-in MCP extension. It owns the `/mcp` command and the three resource tools;
/// per-server tools are registered when the corresponding connection becomes ready.
#[derive(Clone, Debug, Default)]
pub struct McpExtension {
    options: McpExtensionOptions,
}

impl McpExtension {
    pub fn with_options(options: McpExtensionOptions) -> Self {
        Self { options }
    }

    pub fn options(&self) -> McpExtensionOptions {
        self.options
    }
}

impl Extension for McpExtension {
    fn name(&self) -> &'static str {
        "mcp"
    }

    fn register(&self, registry: &mut ExtensionRegistry) {
        registry.register_command("mcp");
        for name in [
            LIST_MCP_RESOURCES_TOOL,
            LIST_MCP_RESOURCE_TEMPLATES_TOOL,
            READ_MCP_RESOURCE_TOOL,
        ] {
            registry.register_tool_with(ToolRegistration {
                name: name.to_string(),
                default_active: false,
                exposure: ToolExposure::Hidden,
            });
        }
    }
}

pub type McpConnector = Arc<
    dyn Fn(
            PathBuf,
            String,
            Value,
            PathBuf,
        ) -> BoxFuture<'static, Result<(McpClient, Vec<pi_rs_mcp::Tool>), String>>
        + Send
        + Sync,
>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpReconnectOutcome {
    pub server: String,
    pub tool_count: usize,
    pub generation: u64,
}

#[derive(Clone)]
struct ConnectedMcpServer {
    client: McpClient,
    tool_names: Vec<String>,
    generation: u64,
}

#[derive(Clone)]
struct RegisteredMcpTool {
    server: String,
    client: McpClient,
    remote_name: String,
    definition: ToolDefinition,
}

#[derive(Default)]
struct McpManagerState {
    servers: IndexMap<String, ConnectedMcpServer>,
    tools: IndexMap<String, RegisteredMcpTool>,
}

struct McpConnectionManagerInner {
    connector: McpConnector,
    reconnect_lock: tokio::sync::Mutex<()>,
    state: Mutex<McpManagerState>,
}

#[derive(Clone)]
pub struct McpConnectionManager {
    inner: Arc<McpConnectionManagerInner>,
}

impl Default for McpConnectionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpConnectionManager {
    pub fn new() -> Self {
        Self::with_connector(Arc::new(|agent_dir, name, config, cwd| {
            Box::pin(
                async move { crate::cli::mcp::discover(&agent_dir, &name, &config, &cwd).await },
            )
        }))
    }

    pub fn with_connector(connector: McpConnector) -> Self {
        Self {
            inner: Arc::new(McpConnectionManagerInner {
                connector,
                reconnect_lock: tokio::sync::Mutex::new(()),
                state: Mutex::new(McpManagerState::default()),
            }),
        }
    }

    pub async fn connect_all(
        &self,
        agent_dir: &Path,
        cwd: &Path,
        project_trusted: bool,
    ) -> Result<(), String> {
        let _guard = self.inner.reconnect_lock.lock().await;
        let loaded = crate::cli::mcp::load_config(agent_dir, cwd, project_trusted);
        for (name, config) in loaded.servers {
            if config["enabled"] == false {
                continue;
            }
            let Ok((client, tools)) = (self.inner.connector)(
                agent_dir.to_path_buf(),
                name.clone(),
                config.clone(),
                cwd.to_path_buf(),
            )
            .await
            else {
                continue;
            };
            self.install_server(&name, &config, client, tools, 1)?;
        }
        Ok(())
    }

    pub async fn disconnect_server(&self, server_name: &str) -> bool {
        let _guard = self.inner.reconnect_lock.lock().await;
        let removed = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            remove_server_locked(&mut state, server_name)
        };
        if let Some(old) = removed {
            old.client.close().await;
            true
        } else {
            false
        }
    }

    pub async fn reconnect_server(
        &self,
        server_name: &str,
        agent_dir: &Path,
        cwd: &Path,
        project_trusted: bool,
    ) -> Result<McpReconnectOutcome, String> {
        let _guard = self.inner.reconnect_lock.lock().await;
        let loaded = crate::cli::mcp::load_config(agent_dir, cwd, project_trusted);
        let Some(config) = loaded.servers.get(server_name).cloned() else {
            return Err(format!("No MCP server named \"{server_name}\"."));
        };
        self.reconnect_server_locked(server_name, &config, agent_dir, cwd)
            .await
    }

    pub async fn reconnect_all(
        &self,
        agent_dir: &Path,
        cwd: &Path,
        project_trusted: bool,
    ) -> Result<Vec<McpReconnectOutcome>, String> {
        let _guard = self.inner.reconnect_lock.lock().await;
        let loaded = crate::cli::mcp::load_config(agent_dir, cwd, project_trusted);
        if loaded
            .servers
            .values()
            .all(|entry| entry["enabled"] == false)
        {
            return Err("No enabled MCP server to reconnect.".to_owned());
        }
        let stale: Vec<String> = {
            let state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state
                .servers
                .keys()
                .filter(|name| {
                    loaded
                        .servers
                        .get(*name)
                        .is_none_or(|entry| entry["enabled"] == false)
                })
                .cloned()
                .collect()
        };
        for name in stale {
            let removed = {
                let mut state = self
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                remove_server_locked(&mut state, &name)
            };
            if let Some(old) = removed {
                old.client.close().await;
            }
        }
        let mut outcomes = Vec::new();
        for (name, config) in loaded.servers {
            if config["enabled"] == false {
                continue;
            }
            outcomes.push(
                self.reconnect_server_locked(&name, &config, agent_dir, cwd)
                    .await?,
            );
        }
        Ok(outcomes)
    }

    async fn reconnect_server_locked(
        &self,
        server_name: &str,
        config: &Value,
        agent_dir: &Path,
        cwd: &Path,
    ) -> Result<McpReconnectOutcome, String> {
        let removed = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            remove_server_locked(&mut state, server_name)
        };
        let next_generation = removed.as_ref().map_or(1, |old| old.generation + 1);
        if let Some(old) = removed {
            old.client.close().await;
        }
        if config["enabled"] == false {
            return Err(format!("MCP server \"{server_name}\" is disabled."));
        }
        let (client, tools) = (self.inner.connector)(
            agent_dir.to_path_buf(),
            server_name.to_owned(),
            config.clone(),
            cwd.to_path_buf(),
        )
        .await?;
        let tool_count =
            self.install_server(server_name, config, client, tools, next_generation)?;
        Ok(McpReconnectOutcome {
            server: server_name.to_owned(),
            tool_count,
            generation: next_generation,
        })
    }

    fn install_server(
        &self,
        server_name: &str,
        config: &Value,
        client: McpClient,
        tools: Vec<pi_rs_mcp::Tool>,
        generation: u64,
    ) -> Result<usize, String> {
        let prepared = tools
            .into_iter()
            .map(|tool| {
                let schema = pi_rs_mcp::normalize_mcp_input_schema(&tool.input_schema)
                    .map_err(|error| error.to_string())?;
                Ok((tool, schema))
            })
            .collect::<Result<Vec<_>, String>>()?;

        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut registered_names = Vec::with_capacity(prepared.len());
        for (tool, schema) in prepared {
            let local =
                create_mcp_tool_name(server_name, &tool.name, |key| state.tools.contains_key(key));
            let mut definition = ToolDefinition::new(local.clone(), schema);
            definition.description = tool.description().unwrap_or_default().to_owned();
            definition.label = tool.title().unwrap_or(&tool.name).to_owned();
            definition.exposure = match config["toolExposure"]
                .get(&tool.name)
                .or_else(|| config.get("exposure"))
                .and_then(Value::as_str)
            {
                Some("direct") => RuntimeToolExposure::Direct,
                Some("deferred") => RuntimeToolExposure::Deferred,
                Some("hidden") => RuntimeToolExposure::Hidden,
                _ => RuntimeToolExposure::Codemode,
            };
            state.tools.insert(
                local.clone(),
                RegisteredMcpTool {
                    server: server_name.to_owned(),
                    client: client.clone(),
                    remote_name: tool.name,
                    definition,
                },
            );
            registered_names.push(local);
        }
        let tool_count = registered_names.len();
        state.servers.insert(
            server_name.to_owned(),
            ConnectedMcpServer {
                client,
                tool_names: registered_names,
                generation,
            },
        );
        Ok(tool_count)
    }

    pub fn close_all(&self) {
        let clients: Vec<McpClient> = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.tools.clear();
            state
                .servers
                .drain(..)
                .map(|(_, server)| server.client)
                .collect()
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            for client in clients {
                handle.spawn(async move {
                    client.close().await;
                });
            }
        }
    }

    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state
            .tools
            .values()
            .map(|entry| entry.definition.clone())
            .collect()
    }

    pub fn agent_tools(&self) -> Vec<Arc<dyn pi_rs_agent::AgentTool>> {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state
            .tools
            .values()
            .filter(|entry| {
                matches!(
                    entry.definition.exposure,
                    RuntimeToolExposure::Direct | RuntimeToolExposure::ModelOnly
                )
            })
            .map(|entry| {
                Arc::new(McpAgentTool {
                    declaration: pi_rs_ai::Tool {
                        name: entry.definition.name.clone(),
                        description: entry.definition.description.clone(),
                        parameters: entry.definition.parameters.clone(),
                        constrained_sampling: None,
                    },
                    label: entry.definition.label.clone(),
                    client: entry.client.clone(),
                    remote: entry.remote_name.clone(),
                }) as Arc<dyn pi_rs_agent::AgentTool>
            })
            .collect()
    }

    pub async fn execute_tool(
        &self,
        name: &str,
        args: Value,
        ctx: &ExtensionToolContext,
    ) -> Result<ToolOutcome, String> {
        let (client, remote) = {
            let state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let entry = state
                .tools
                .get(name)
                .ok_or_else(|| format!("Unknown MCP tool {name}"))?;
            (entry.client.clone(), entry.remote_name.clone())
        };
        let reply = client
            .call_tool(&remote, Some(args.clone()), &Default::default())
            .await
            .map_err(|error| error.to_string())?;
        let failed = reply.is_error.unwrap_or(false);
        let mut outcome = ToolOutcome::success(
            serde_json::json!({"type":"toolCall","id":ctx.tool_call_id,"name":name,"arguments":args}),
            serde_json::to_value(reply).map_err(|error| error.to_string())?,
        );
        outcome.is_error = failed;
        Ok(outcome)
    }

    pub fn server_client(&self, server_name: &str) -> Option<McpClient> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .servers
            .get(server_name)
            .map(|server| server.client.clone())
    }

    pub fn server_generation(&self, server_name: &str) -> Option<u64> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .servers
            .get(server_name)
            .map(|server| server.generation)
    }

    pub fn server_names(&self) -> Vec<String> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .servers
            .keys()
            .cloned()
            .collect()
    }

    pub fn connected_clients(&self) -> IndexMap<String, McpClient> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .servers
            .iter()
            .map(|(name, server)| (name.clone(), server.client.clone()))
            .collect()
    }

    pub fn servers_section(
        &self,
        agent_dir: &Path,
        cwd: &Path,
        project_trusted: bool,
    ) -> Option<String> {
        let loaded = crate::cli::mcp::load_config(agent_dir, cwd, project_trusted);
        let clients = self.connected_clients();
        let listings: Vec<McpServerListing> = loaded
            .servers
            .iter()
            .map(|(name, entry)| {
                let mut config = McpServerConfig::new(name);
                if entry["enabled"] == false {
                    config = config.disabled();
                }
                if let Some(exposure) = entry["exposure"].as_str().and_then(McpExposure::parse) {
                    config = config.with_exposure(exposure);
                }
                if let Some(tool_exp) = entry["toolExposure"].as_object() {
                    for (tool_name, exp_val) in tool_exp {
                        if let Some(exp) = exp_val.as_str().and_then(McpExposure::parse) {
                            config = config.with_tool_exposure(tool_name, exp);
                        }
                    }
                }
                if let Some(desc) = entry["description"].as_str() {
                    config = config.with_description(desc);
                }
                if let Some(instr) = clients.get(name).and_then(McpClient::instructions) {
                    config = config.with_instructions(instr);
                }
                McpServerListing { config }
            })
            .collect();
        render_servers_section(&listings)
    }

    pub async fn execute_resource_tool(
        &self,
        name: &str,
        args: &Value,
        ctx: &ExtensionToolContext,
    ) -> Option<Result<ToolOutcome, String>> {
        match name {
            LIST_MCP_RESOURCES_TOOL => Some(self.run_list_resources(args, ctx).await),
            LIST_MCP_RESOURCE_TEMPLATES_TOOL => {
                Some(self.run_list_resource_templates(args, ctx).await)
            }
            READ_MCP_RESOURCE_TOOL => Some(self.run_read_resource(args, ctx).await),
            _ => None,
        }
    }

    async fn run_list_resources(
        &self,
        args: &Value,
        ctx: &ExtensionToolContext,
    ) -> Result<ToolOutcome, String> {
        let target_server = args.get("server").and_then(Value::as_str);
        let cursor = args.get("cursor").and_then(Value::as_str);
        let clients = self.connected_clients();
        let mut resources = Vec::new();
        let mut errors = Vec::new();
        let mut next_cursor = None;

        if let Some(server) = target_server {
            let Some(client) = clients.get(server) else {
                return Ok(ToolOutcome::error(
                    &ctx.tool_call_id,
                    LIST_MCP_RESOURCES_TOOL,
                    &format!("Unknown or disconnected MCP server \"{server}\"."),
                ));
            };
            match client
                .list_resources_page(cursor, &Default::default())
                .await
            {
                Ok(page) => {
                    next_cursor = page.next_cursor;
                    for r in page.resources {
                        let mut val = serde_json::to_value(r).unwrap_or(Value::Null);
                        if let Some(obj) = val.as_object_mut() {
                            obj.insert("server".to_owned(), serde_json::json!(server));
                        }
                        resources.push(val);
                    }
                }
                Err(error) => errors.push(serde_json::json!({
                    "server": server,
                    "error": error.to_string(),
                })),
            }
        } else {
            for (server, client) in &clients {
                match client.list_resources(&Default::default()).await {
                    Ok(page) => {
                        for r in page {
                            let mut val = serde_json::to_value(r).unwrap_or(Value::Null);
                            if let Some(obj) = val.as_object_mut() {
                                obj.insert("server".to_owned(), serde_json::json!(server));
                            }
                            resources.push(val);
                        }
                    }
                    Err(error) => errors.push(serde_json::json!({
                        "server": server,
                        "error": error.to_string(),
                    })),
                }
            }
        }
        let mut structured = serde_json::json!({ "resources": resources });
        if let Some(server) = target_server {
            structured["server"] = serde_json::json!(server);
        }
        if let Some(cursor) = next_cursor {
            structured["nextCursor"] = serde_json::json!(cursor);
        }
        if !errors.is_empty() {
            structured["errors"] = serde_json::json!(errors);
        }
        let summary = serde_json::to_string_pretty(&structured).unwrap_or_else(|_| "{}".into());
        Ok(ToolOutcome::success(
            serde_json::json!({ "type": "toolCall", "id": ctx.tool_call_id, "name": LIST_MCP_RESOURCES_TOOL, "arguments": args }),
            serde_json::json!({
                "content": [{ "type": "text", "text": summary }],
                "structuredContent": structured,
            }),
        ))
    }

    async fn run_list_resource_templates(
        &self,
        args: &Value,
        ctx: &ExtensionToolContext,
    ) -> Result<ToolOutcome, String> {
        let target_server = args.get("server").and_then(Value::as_str);
        let cursor = args.get("cursor").and_then(Value::as_str);
        let clients = self.connected_clients();
        let mut templates = Vec::new();
        let mut errors = Vec::new();
        let mut next_cursor = None;

        if let Some(server) = target_server {
            let Some(client) = clients.get(server) else {
                return Ok(ToolOutcome::error(
                    &ctx.tool_call_id,
                    LIST_MCP_RESOURCE_TEMPLATES_TOOL,
                    &format!("Unknown or disconnected MCP server \"{server}\"."),
                ));
            };
            match client
                .list_resource_templates_page(cursor, &Default::default())
                .await
            {
                Ok(page) => {
                    next_cursor = page.next_cursor;
                    for t in page.resource_templates {
                        let mut val = serde_json::to_value(t).unwrap_or(Value::Null);
                        if let Some(obj) = val.as_object_mut() {
                            obj.insert("server".to_owned(), serde_json::json!(server));
                        }
                        templates.push(val);
                    }
                }
                Err(error) => errors.push(serde_json::json!({
                    "server": server,
                    "error": error.to_string(),
                })),
            }
        } else {
            for (server, client) in &clients {
                match client.list_resource_templates(&Default::default()).await {
                    Ok(page) => {
                        for t in page {
                            let mut val = serde_json::to_value(t).unwrap_or(Value::Null);
                            if let Some(obj) = val.as_object_mut() {
                                obj.insert("server".to_owned(), serde_json::json!(server));
                            }
                            templates.push(val);
                        }
                    }
                    Err(error) => errors.push(serde_json::json!({
                        "server": server,
                        "error": error.to_string(),
                    })),
                }
            }
        }
        let mut structured = serde_json::json!({ "resourceTemplates": templates });
        if let Some(server) = target_server {
            structured["server"] = serde_json::json!(server);
        }
        if let Some(cursor) = next_cursor {
            structured["nextCursor"] = serde_json::json!(cursor);
        }
        if !errors.is_empty() {
            structured["errors"] = serde_json::json!(errors);
        }
        let summary = serde_json::to_string_pretty(&structured).unwrap_or_else(|_| "{}".into());
        Ok(ToolOutcome::success(
            serde_json::json!({ "type": "toolCall", "id": ctx.tool_call_id, "name": LIST_MCP_RESOURCE_TEMPLATES_TOOL, "arguments": args }),
            serde_json::json!({
                "content": [{ "type": "text", "text": summary }],
                "structuredContent": structured,
            }),
        ))
    }

    async fn run_read_resource(
        &self,
        args: &Value,
        ctx: &ExtensionToolContext,
    ) -> Result<ToolOutcome, String> {
        let Some(server) = args.get("server").and_then(Value::as_str) else {
            return Ok(ToolOutcome::error(
                &ctx.tool_call_id,
                READ_MCP_RESOURCE_TOOL,
                "Missing required parameter \"server\".",
            ));
        };
        let Some(uri) = args.get("uri").and_then(Value::as_str) else {
            return Ok(ToolOutcome::error(
                &ctx.tool_call_id,
                READ_MCP_RESOURCE_TOOL,
                "Missing required parameter \"uri\".",
            ));
        };
        let Some(client) = self.server_client(server) else {
            return Ok(ToolOutcome::error(
                &ctx.tool_call_id,
                READ_MCP_RESOURCE_TOOL,
                &format!("Unknown or disconnected MCP server \"{server}\"."),
            ));
        };
        let result = match client.read_resource(uri, &Default::default()).await {
            Ok(result) => result,
            Err(error) => {
                return Ok(ToolOutcome::error(
                    &ctx.tool_call_id,
                    READ_MCP_RESOURCE_TOOL,
                    &error.to_string(),
                ));
            }
        };
        let raw_blocks: Vec<pi_rs_mcp::LlmContent> = result
            .contents
            .iter()
            .filter_map(|item| {
                if let Some(text) = pi_rs_mcp::resource_contents_text(item) {
                    Some(pi_rs_mcp::LlmContent::Text {
                        text: text.to_owned(),
                    })
                } else if let Some(blob) = item.get("blob").and_then(Value::as_str) {
                    let mime = item
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("application/octet-stream");
                    if mime.starts_with("image/") {
                        Some(pi_rs_mcp::LlmContent::Image {
                            data: blob.to_owned(),
                            mime_type: mime.to_owned(),
                        })
                    } else {
                        Some(pi_rs_mcp::LlmContent::Text {
                            text: format!(
                                "[Binary resource ({mime}, {} bytes base64)]",
                                blob.len()
                            ),
                        })
                    }
                } else {
                    None
                }
            })
            .collect();
        let limited = pi_rs_mcp::limit_mcp_content(raw_blocks, None).await;
        let content: Vec<Value> = limited
            .content
            .into_iter()
            .map(|block| match block {
                pi_rs_mcp::LlmContent::Text { text } => {
                    serde_json::json!({ "type": "text", "text": text })
                }
                pi_rs_mcp::LlmContent::Image { data, mime_type } => {
                    serde_json::json!({ "type": "image", "data": data, "mimeType": mime_type })
                }
            })
            .collect();
        Ok(ToolOutcome::success(
            serde_json::json!({ "type": "toolCall", "id": ctx.tool_call_id, "name": READ_MCP_RESOURCE_TOOL, "arguments": args }),
            serde_json::json!({
                "content": content,
                "structuredContent": serde_json::json!({ "contents": result.contents }),
            }),
        ))
    }
}

pub fn mcp_resource_tool_definitions() -> Vec<ToolDefinition> {
    let mut list_resources = ToolDefinition::new(
        LIST_MCP_RESOURCES_TOOL,
        pi_rs_mcp::create_mcp_result_schema(None),
    );
    list_resources.label = LIST_MCP_RESOURCES_TOOL.to_owned();
    list_resources.description = "List resources exposed by connected MCP servers.".to_owned();
    list_resources.parameters = serde_json::json!({
        "type": "object",
        "properties": {
            "server": { "type": "string", "description": "Optional MCP server name." },
            "cursor": { "type": "string", "description": "Optional pagination cursor." },
        },
        "additionalProperties": false,
    });
    list_resources.exposure = RuntimeToolExposure::ModelOnly;

    let mut list_templates = ToolDefinition::new(
        LIST_MCP_RESOURCE_TEMPLATES_TOOL,
        serde_json::json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "description": "Optional MCP server name." },
                "cursor": { "type": "string", "description": "Optional pagination cursor." },
            },
            "additionalProperties": false,
        }),
    );
    list_templates.label = LIST_MCP_RESOURCE_TEMPLATES_TOOL.to_owned();
    list_templates.description =
        "List resource templates exposed by connected MCP servers.".to_owned();
    list_templates.exposure = RuntimeToolExposure::ModelOnly;

    let mut read_resource = ToolDefinition::new(
        READ_MCP_RESOURCE_TOOL,
        serde_json::json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "description": "MCP server name." },
                "uri": { "type": "string", "description": "Resource URI to read." },
            },
            "required": ["server", "uri"],
            "additionalProperties": false,
        }),
    );
    read_resource.label = READ_MCP_RESOURCE_TOOL.to_owned();
    read_resource.description = "Read a resource from an MCP server by URI.".to_owned();
    read_resource.exposure = RuntimeToolExposure::ModelOnly;

    vec![list_resources, list_templates, read_resource]
}

pub fn update_mcp_server_config_entry(
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    server_name: &str,
    enabled: Option<bool>,
    exposure: Option<&str>,
) -> Result<(), String> {
    let project_path = cwd.join(crate::config::CONFIG_DIR_NAME).join("mcp.json");
    let global_path = agent_dir.join("mcp.json");
    let target_path = if project_trusted
        && project_path.is_file()
        && std::fs::read_to_string(&project_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .is_some_and(|v| v["mcpServers"].get(server_name).is_some())
    {
        project_path
    } else {
        global_path
    };
    let mut doc = if target_path.is_file() {
        let raw = std::fs::read_to_string(&target_path).map_err(|e| e.to_string())?;
        serde_json::from_str::<Value>(&raw).map_err(|e| e.to_string())?
    } else {
        serde_json::json!({ "mcpServers": {} })
    };
    let Some(entry) = doc["mcpServers"]
        .get_mut(server_name)
        .and_then(Value::as_object_mut)
    else {
        return Err(format!("No MCP server named \"{server_name}\" in config."));
    };
    if let Some(flag) = enabled {
        entry.insert("enabled".to_owned(), serde_json::json!(flag));
    }
    if let Some(exp) = exposure {
        entry.insert("exposure".to_owned(), serde_json::json!(exp));
    }
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let formatted = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    std::fs::write(&target_path, formatted).map_err(|e| e.to_string())
}

fn remove_server_locked(
    state: &mut McpManagerState,
    server_name: &str,
) -> Option<ConnectedMcpServer> {
    let removed = state.servers.shift_remove(server_name);
    if let Some(old) = &removed {
        for tool_name in &old.tool_names {
            state.tools.shift_remove(tool_name);
        }
    } else {
        state.tools.retain(|_, entry| entry.server != server_name);
    }
    removed
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
            .map_err(|error| AgentError::Callback(error.to_string()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposure_parsing_and_tool_exposure() {
        assert_eq!(McpExposure::parse("direct"), Some(McpExposure::Direct));
        assert_eq!(McpExposure::parse("hidden"), Some(McpExposure::Hidden));
        assert_eq!(McpExposure::parse("unknown"), None);
        assert_eq!(McpExposure::Direct.tool_exposure(), ToolExposure::Model);
        assert_eq!(McpExposure::Hidden.tool_exposure(), ToolExposure::Hidden);
    }

    #[test]
    fn tool_names_use_upstream_sanitization() {
        let config = McpServerConfig::new("docs-server");
        assert_eq!(config.namespace(), "mcp__docs_server");
        assert_eq!(
            config.tool_name("read/file", |_| false),
            "mcp__docs_server__read_file"
        );
    }

    #[test]
    fn taken_names_get_hash_suffix() {
        let config = McpServerConfig::new("docs");
        let first = config.tool_name("search", |_| false);
        let second = config.tool_name("search", |name| name == first);
        assert_ne!(first, second);
        assert!(second.starts_with(&first[..10]));
    }

    #[test]
    fn per_tool_exposure_overrides_server_default() {
        let config = McpServerConfig::new("docs")
            .with_exposure(McpExposure::Codemode)
            .with_tool_exposure("status", McpExposure::Direct);
        assert_eq!(config.exposure_for_tool("status"), McpExposure::Direct);
        assert_eq!(config.exposure_for_tool("other"), McpExposure::Codemode);
        assert!(config.has_direct_tools());
        assert!(config.has_indirect_tools());
    }

    #[test]
    fn hidden_server_is_not_listed() {
        let section = render_servers_section(&[McpServerListing {
            config: McpServerConfig::new("docs").disabled(),
        }]);
        assert!(section.is_none());
    }

    #[test]
    fn section_lists_indirect_servers_with_reach() {
        let section = render_servers_section(&[
            McpServerListing {
                config: McpServerConfig::new("docs")
                    .with_description("Documentation search")
                    .with_exposure(McpExposure::Deferred),
            },
            McpServerListing {
                config: McpServerConfig::new("ui").with_exposure(McpExposure::Codemode),
            },
        ]);
        let section = section.expect("section for indirect servers");
        assert!(section.contains("MCP servers whose tools are not declared to you."));
        assert!(section.contains("- mcp__docs (tool_search): Documentation search"));
        assert!(section.contains("- mcp__ui (codemode)"));
        assert!(section.contains("codemode scripts"));
        assert!(section.contains("tool_search"));
    }

    #[test]
    fn reaches_server_matches_namespace() {
        let reach = vec!["mcp__docs_server__read".to_string()];
        assert!(reaches_server(&reach, "docs-server"));
        assert!(!reaches_server(&reach, "other"));
    }

    #[test]
    fn extension_registers_manager_command_and_resource_tools() {
        let mut registry = ExtensionRegistry::default();
        McpExtension::default().register(&mut registry);
        assert_eq!(registry.commands, ["mcp"]);
        assert_eq!(registry.tools.len(), 3);
        assert!(registry.tools.iter().all(|tool| !tool.default_active));
    }

    #[test]
    fn startup_wait_default_matches_upstream() {
        assert_eq!(McpExtensionOptions::default().startup_wait_ms(), 10_000);
    }

    async fn spawn_test_mcp_http_server(
        server_name: &'static str,
        tools_by_init: Vec<Vec<&'static str>>,
    ) -> Result<
        (
            String,
            Arc<std::sync::atomic::AtomicUsize>,
            Arc<std::sync::atomic::AtomicUsize>,
            tokio::task::JoinHandle<()>,
        ),
        Box<dyn std::error::Error>,
    > {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let init_count = Arc::new(AtomicUsize::new(0));
        let delete_count = Arc::new(AtomicUsize::new(0));
        let inits = init_count.clone();
        let deletes = delete_count.clone();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let inits = inits.clone();
                let deletes = deletes.clone();
                let tools_by_init = tools_by_init.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(socket);
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let mut length = 0usize;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':')
                            && name.eq_ignore_ascii_case("content-length")
                        {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                    if request_line.starts_with("DELETE ") {
                        deletes.fetch_add(1, Ordering::SeqCst);
                        let _ = reader
                            .get_mut()
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await;
                        return;
                    }
                    let mut bytes = vec![0; length];
                    if reader.read_exact(&mut bytes).await.is_err() {
                        return;
                    }
                    let Ok(request) = serde_json::from_slice::<Value>(&bytes) else {
                        return;
                    };
                    let Some(method) = request["method"].as_str() else {
                        return;
                    };
                    let result = match method {
                        "initialize" => {
                            inits.fetch_add(1, Ordering::SeqCst);
                            serde_json::json!({
                                "protocolVersion": pi_rs_mcp::LATEST_PROTOCOL_VERSION,
                                "capabilities": { "tools": {} },
                                "serverInfo": { "name": server_name, "version": "1" }
                            })
                        }
                        "notifications/initialized" => {
                            let _ = reader
                                .get_mut()
                                .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                .await;
                            return;
                        }
                        "tools/list" => {
                            let current = inits.load(Ordering::SeqCst).saturating_sub(1);
                            let tool_names = tools_by_init
                                .get(current)
                                .or_else(|| tools_by_init.last())
                                .cloned()
                                .unwrap_or_default();
                            let tools: Vec<Value> = tool_names
                                .into_iter()
                                .map(|name| {
                                    serde_json::json!({
                                        "name": name,
                                        "inputSchema": { "type": "object", "properties": {} }
                                    })
                                })
                                .collect();
                            serde_json::json!({ "tools": tools })
                        }
                        "tools/call" => {
                            let name = request["params"]["name"].as_str().unwrap_or("unknown");
                            let generation = inits.load(Ordering::SeqCst);
                            serde_json::json!({
                                "content": [{ "type": "text", "text": format!("{server_name}:{name}:gen{generation}") }]
                            })
                        }
                        _ => return,
                    };
                    let response = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": result
                    })
                    .to_string();
                    let _ = reader
                        .get_mut()
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{response}",
                                response.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        Ok((url, init_count, delete_count, handle))
    }

    #[tokio::test]
    async fn single_server_reconnect_refreshes_only_target_server_and_preserves_other()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::core::{
            event_bus::EventBus,
            extensions::{
                context::UiInject,
                runner::ExtensionRunner,
                types::{
                    ExtensionCommandContext, ExtensionContext, ExtensionHost, ExtensionMode,
                    ExtensionUi,
                },
            },
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (alpha_url, alpha_inits, _alpha_deletes, alpha_task) =
            spawn_test_mcp_http_server("alpha", vec![vec!["ping_v1"], vec!["ping_v2"]]).await?;
        let (beta_url, beta_inits, beta_deletes, beta_task) =
            spawn_test_mcp_http_server("beta", vec![vec!["echo_b"]]).await?;

        let dir = tempfile::tempdir()?;
        let config = serde_json::json!({
            "mcpServers": {
                "alpha": { "url": alpha_url, "exposure": "direct", "headers": { "Authorization": "Bearer alpha-token" } },
                "beta": { "url": beta_url, "exposure": "direct", "headers": { "Authorization": "Bearer beta-token" } }
            }
        });
        std::fs::write(dir.path().join("mcp.json"), serde_json::to_vec(&config)?)?;

        let bus = Arc::new(EventBus::new());
        let host = Arc::new(ExtensionHost::new(bus.clone()));
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let refresh_counter = refresh_calls.clone();
        host.set_refresh_tools(Arc::new(move || {
            refresh_counter.fetch_add(1, Ordering::SeqCst);
        }));

        let runner = Arc::new(ExtensionRunner::new(
            Vec::new(),
            host.clone(),
            bus.clone(),
            dir.path().to_string_lossy(),
        ));
        let factory = crate::extensions::headless::factory("builtin:mcp", dir.path(), false)
            .ok_or("Missing builtin:mcp factory")?;
        let loaded = runner.load_extensions(&[factory]).await;
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

        assert_eq!(alpha_inits.load(Ordering::SeqCst), 1);
        assert_eq!(beta_inits.load(Ordering::SeqCst), 1);
        let initial_names: Vec<String> = runner
            .get_all_registered_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(initial_names.contains(&"mcp__alpha__ping_v1".to_owned()));
        assert!(initial_names.contains(&"mcp__beta__echo_b".to_owned()));

        let reload_calls = Arc::new(AtomicUsize::new(0));
        let reload_counter = reload_calls.clone();
        let notices = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let notice_log = notices.clone();
        let ui = ExtensionUi::from_inject(
            ExtensionMode::Tui,
            UiInject {
                notify: Arc::new(move |msg, kind| {
                    notice_log
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((msg, kind));
                }),
                ..UiInject::default()
            },
        );
        let cmd_ctx = ExtensionCommandContext {
            base: ExtensionContext {
                host: host.clone(),
                cwd: dir.path().to_string_lossy().into_owned(),
                mode: ExtensionMode::Tui,
                ui,
                bus: Some(bus),
                is_idle: Arc::new(|| true),
                is_project_trusted: Arc::new(|| false),
                has_pending_messages: Arc::new(|| false),
                get_context_usage: Arc::new(|| None),
                get_system_prompt: Arc::new(String::new),
                abort: Arc::new(|| {}),
                shutdown: Arc::new(|| {}),
                compact: Arc::new(|_| {}),
            },
            wait_for_idle: Arc::new(|| Box::pin(async {})),
            new_session: Arc::new(|_| Box::pin(async { Ok(Value::Null) })),
            fork: Arc::new(|_, _| Box::pin(async { Ok(Value::Null) })),
            navigate_tree: Arc::new(|_, _| Box::pin(async { Ok(Value::Null) })),
            switch_session: Arc::new(|_, _| Box::pin(async { Ok(Value::Null) })),
            reload: Arc::new(move || {
                let reload_counter = reload_counter.clone();
                Box::pin(async move {
                    reload_counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }),
        };

        // Reconnect ONLY alpha.
        runner
            .execute_command("mcp", "reconnect alpha", &cmd_ctx)
            .await?;

        assert_eq!(
            reload_calls.load(Ordering::SeqCst),
            0,
            "single-server reconnect must not trigger full session reload"
        );
        assert!(refresh_calls.load(Ordering::SeqCst) >= 1);
        assert_eq!(
            alpha_inits.load(Ordering::SeqCst),
            2,
            "alpha must reconnect once"
        );
        assert_eq!(
            beta_inits.load(Ordering::SeqCst),
            1,
            "beta must NOT reconnect"
        );
        assert_eq!(
            beta_deletes.load(Ordering::SeqCst),
            0,
            "beta must NOT be closed/deleted"
        );

        let updated_names: Vec<String> = runner
            .get_all_registered_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(!updated_names.contains(&"mcp__alpha__ping_v1".to_owned()));
        assert!(updated_names.contains(&"mcp__alpha__ping_v2".to_owned()));
        assert!(updated_names.contains(&"mcp__beta__echo_b".to_owned()));

        let agent_tools = runner.extensions()[0].agent_tools();
        let alpha_tool = agent_tools
            .iter()
            .find(|tool| tool.declaration().name == "mcp__alpha__ping_v2")
            .ok_or("missing refreshed alpha tool")?;
        let beta_tool = agent_tools
            .iter()
            .find(|tool| tool.declaration().name == "mcp__beta__echo_b")
            .ok_or("missing untouched beta tool")?;

        let cancel = pi_rs_agent::CancellationToken::new();
        let alpha_res = alpha_tool
            .execute("call-a", &serde_json::json!({}), &cancel, Arc::new(|_| {}))
            .await?;
        let beta_res = beta_tool
            .execute("call-b", &serde_json::json!({}), &cancel, Arc::new(|_| {}))
            .await?;
        assert!(matches!(
            alpha_res.content.first(),
            Some(pi_rs_ai::InputContent::Text(t)) if t.text == "alpha:ping_v2:gen2"
        ));
        assert!(matches!(
            beta_res.content.first(),
            Some(pi_rs_ai::InputContent::Text(t)) if t.text == "beta:echo_b:gen1"
        ));

        let recorded_notices = notices.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            recorded_notices.iter().any(|(msg, kind)| msg
                .contains("Reconnected MCP server \"alpha\"")
                && kind == "info"),
            "{recorded_notices:?}"
        );

        runner.invalidate();
        alpha_task.abort();
        beta_task.abort();
        Ok(())
    }

    #[test]
    fn resource_tool_definitions_match_exact_three_tools() {
        let defs = mcp_resource_tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            [
                LIST_MCP_RESOURCES_TOOL,
                LIST_MCP_RESOURCE_TEMPLATES_TOOL,
                READ_MCP_RESOURCE_TOOL,
            ]
        );
    }

    #[test]
    fn servers_section_uses_configured_indirect_servers() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let agent_dir = dir.path().join("agent");
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(cwd.join(".pi")).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&agent_dir).map_err(|e| e.to_string())?;
        std::fs::write(
            cwd.join(".pi").join("mcp.json"),
            r#"{"mcpServers":{"docs":{"url":"https://docs.example/mcp","exposure":"deferred"}}}"#,
        )
        .map_err(|e| e.to_string())?;

        let manager = McpConnectionManager::default();
        let section = manager
            .servers_section(&agent_dir, &cwd, true)
            .ok_or("expected servers section")?;
        assert!(section.contains("- mcp__docs (tool_search)"));
        Ok(())
    }
}
