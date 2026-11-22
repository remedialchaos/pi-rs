//! The built-in MCP extension adapter.
//!
//! Upstream (`packages/coding-agent/src/extensions/mcp/`) connects servers from `mcp.json`,
//! registers their tools as `mcp__<server>__<tool>`, and exposes a `/mcp` manager. Transport and
//! OAuth live in `pi_rs_mcp`; this module owns the coding-agent-facing contract: server config,
//! exposure resolution, tool naming, and the `mcp_servers` system-prompt section.

use std::collections::{BTreeMap, BTreeSet};

use pi_rs_mcp::{create_mcp_tool_name, mcp_namespace, mcp_server_summary};

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
}
