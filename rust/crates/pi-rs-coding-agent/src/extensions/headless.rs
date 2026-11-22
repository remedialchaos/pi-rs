//! Command adapters for the non-interactive extension host.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use indexmap::IndexMap;
use pi_rs_mcp::{
    McpClient, McpClientOptions, McpServerConfigOptions, McpServerConfigResult, McpTransport,
    StdioTransport, StdioTransportOptions, StreamableHttpTransport, StreamableHttpTransportOptions,
    validate_mcp_server_config,
};
use serde_json::Value;

use crate::{
    config::VERSION,
    core::extensions::types::{
        CommandDefinition, Extension, ExtensionFactory, ExtensionInitContext, ExtensionRegistration,
    },
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
    command: Command,
    registration: ExtensionRegistration,
}

#[async_trait]
impl Extension for HeadlessExtension {
    fn name(&self) -> &str {
        self.command.path()
    }
    fn replaceable(&self) -> bool {
        matches!(self.command, Command::Mcp { .. })
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
                                mcp_command(&agent_dir, &cwd, project_trusted, &args).await;
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
    let mut servers = IndexMap::<String, Value>::new();
    let mut diagnostics = Vec::new();
    let global = agent_dir.join("mcp.json");
    let project = cwd.join(".pi/mcp.json");
    for path in std::iter::once(&global).chain(project_trusted.then_some(&project)) {
        match tokio::fs::read(path).await {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(config) => {
                    if let Some(entries) = config["mcpServers"].as_object() {
                        for (name, value) in entries {
                            if pi_rs_mcp::is_mcp_server_override(value) {
                                if let Some(base) = servers.get_mut(name) {
                                    for key in ["enabled", "exposure", "toolExposure"] {
                                        if let Some(value) = value.get(key) {
                                            base[key] = value.clone();
                                        }
                                    }
                                }
                            } else {
                                servers.insert(name.clone(), value.clone());
                            }
                        }
                    }
                }
                Err(error) => {
                    diagnostics.push(format!("config error: {}: {error}", path.display()))
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => diagnostics.push(format!("config error: {}: {error}", path.display())),
        }
    }
    match args.as_slice() {
        ["login" | "logout", ..] if servers.is_empty() => {
            return ("No MCP server uses OAuth sign-in.".to_owned(), "warning");
        }
        ["login" | "logout", ..] => {
            return (
                "MCP OAuth is managed by the MCP transport's configured credential provider."
                    .to_owned(),
                "warning",
            );
        }
        ["reconnect", name] => {
            let Some(server) = servers.get(*name) else {
                return (format!("No MCP server named \"{name}\"."), "warning");
            };
            return match inspect(name, server, cwd).await {
                Ok(tools) => (
                    format!("Reconnected to MCP server \"{name}\" (connected, {tools} tools)."),
                    "info",
                ),
                Err(error) => (error, "error"),
            };
        }
        ["reconnect"] if servers.values().all(|server| server["enabled"] == false) => {
            return ("No enabled MCP server to reconnect.".to_owned(), "warning");
        }
        _ => {}
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
            match inspect(name, server, cwd).await {
                Ok(tools) => format!("{name}: connected, {tools} tools ({exposure})"),
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

async fn inspect(name: &str, server: &Value, cwd: &Path) -> Result<usize, String> {
    let transport: Arc<dyn McpTransport> =
        match validate_mcp_server_config(name, server, &McpServerConfigOptions::default()) {
            McpServerConfigResult::Stdio(server) => StdioTransport::new(StdioTransportOptions {
                command: server["command"]
                    .as_str()
                    .ok_or("Missing command")?
                    .to_owned(),
                args: server["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                cwd: Some(server["cwd"].as_str().map_or_else(
                    || cwd.to_string_lossy().into_owned(),
                    |path| cwd.join(path).to_string_lossy().into_owned(),
                )),
                env: string_map(&server["env"]),
                ..StdioTransportOptions::default()
            }),
            McpServerConfigResult::Http(server) => {
                StreamableHttpTransport::new(StreamableHttpTransportOptions {
                    url: server["url"].as_str().ok_or("Missing URL")?.to_owned(),
                    headers: string_map(&server["headers"]),
                    open_get_stream: Some(false),
                    ..StreamableHttpTransportOptions::default()
                })
                .map_err(|error| error.to_string())?
            }
            McpServerConfigResult::Invalid(message) => return Err(message),
        };
    let client = McpClient::new(McpClientOptions {
        name: "pi".to_owned(),
        version: VERSION.to_owned(),
        ..McpClientOptions::default()
    });
    let result = async {
        let initialized = client.connect(transport).await?;
        if initialized.capabilities.tools.is_none() {
            return Ok(0);
        }
        Ok::<_, pi_rs_mcp::McpClientError>(client.list_tools(&Default::default()).await?.len())
    }
    .await;
    client.close().await;
    result.map_err(|error| error.to_string())
}

fn string_map(value: &Value) -> Option<IndexMap<String, String>> {
    Some(
        value
            .as_object()?
            .iter()
            .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
            .collect(),
    )
}
