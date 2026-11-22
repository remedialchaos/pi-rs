//! Built-in extensions for the coding agent.
//!
//! Upstream keeps four compiled-in extensions in `packages/coding-agent/src/extensions/index.ts`:
//! `builtin:llama.cpp`, `builtin:codemode`, `builtin:tool-search`, and `builtin:mcp`. The three
//! discovery-related ones are replaceable and hidden from the extension list; `llama.cpp` is a
//! permanent provider registration. A non-replaceable extension that registers a name owned by a
//! replaceable built-in causes the built-in to be omitted with a warning
//! (`omitReplacedExtensions()` in `resource-loader.ts`).
//!
//! The `Extension` trait in this module is the local adapter boundary. The shared trait being
//! written in `crate::core::extensions::types` can replace it at integration time without
//! changing the registration and collision logic below.

pub mod codemode;
pub(crate) mod headless;
pub mod llama;
pub mod mcp;
pub mod tool_search;

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Where a tool is exposed to the model, matching upstream's `ToolExposure`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolExposure {
    #[default]
    Model,
    ModelOnly,
    Codemode,
    Deferred,
    Hidden,
}

impl ToolExposure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::ModelOnly => "model-only",
            Self::Codemode => "codemode",
            Self::Deferred => "deferred",
            Self::Hidden => "hidden",
        }
    }
}

/// A tool registered by an extension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRegistration {
    pub name: String,
    pub default_active: bool,
    pub exposure: ToolExposure,
}

impl ToolRegistration {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            default_active: true,
            exposure: ToolExposure::Model,
        }
    }
}

/// Mutable registration surface handed to an extension at load time.
#[derive(Clone, Debug, Default)]
pub struct ExtensionRegistry {
    pub tools: Vec<ToolRegistration>,
    pub commands: Vec<String>,
    pub flags: Vec<String>,
    pub providers: Vec<String>,
}

impl ExtensionRegistry {
    pub fn register_tool(&mut self, name: impl Into<String>) {
        self.tools.push(ToolRegistration::new(name));
    }

    pub fn register_tool_with(&mut self, registration: ToolRegistration) {
        self.tools.push(registration);
    }

    pub fn register_command(&mut self, name: impl Into<String>) {
        self.commands.push(name.into());
    }

    pub fn register_flag(&mut self, name: impl Into<String>) {
        self.flags.push(name.into());
    }

    pub fn register_provider(&mut self, name: impl Into<String>) {
        self.providers.push(name.into());
    }

    /// First registration for a name wins, as in upstream's tool registry.
    pub fn register_tool_first_wins(&mut self, registration: ToolRegistration) -> bool {
        if self.tools.iter().any(|tool| tool.name == registration.name) {
            return false;
        }
        self.tools.push(registration);
        true
    }

    pub fn registration_keys(&self) -> Vec<RegistrationKey> {
        let mut keys = Vec::new();
        keys.extend(
            self.tools
                .iter()
                .map(|tool| RegistrationKey::tool(&tool.name)),
        );
        keys.extend(self.commands.iter().map(RegistrationKey::command));
        keys.extend(self.flags.iter().map(RegistrationKey::flag));
        keys
    }
}

/// Local adapter for the shared `Extension` trait. Built-ins and future WASM extensions both
/// register their tools, commands, flags, and providers through [`ExtensionRegistry`].
pub trait Extension: Send + Sync {
    fn name(&self) -> &'static str;
    fn register(&self, registry: &mut ExtensionRegistry);
}

/// A compiled-in extension descriptor.
#[derive(Clone, Copy, Debug)]
pub struct BuiltinExtension {
    pub name: &'static str,
    pub replaceable: bool,
    pub hidden: bool,
    factory: fn() -> Arc<dyn Extension>,
}

impl PartialEq for BuiltinExtension {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.replaceable == other.replaceable
            && self.hidden == other.hidden
    }
}

impl Eq for BuiltinExtension {}

impl BuiltinExtension {
    pub fn instantiate(&self) -> Arc<dyn Extension> {
        (self.factory)()
    }

    /// Names this built-in claims. Collision with one of them triggers replacement.
    pub fn owned_keys(&self) -> Vec<RegistrationKey> {
        match self.name {
            "codemode" => vec![RegistrationKey::tool("codemode")],
            "tool-search" => vec![RegistrationKey::tool("tool_search")],
            "mcp" => vec![RegistrationKey::command("mcp")],
            "llama.cpp" => vec![RegistrationKey::command("llama")],
            other => vec![RegistrationKey::tool(other)],
        }
    }
}

/// Upstream order: `llama.cpp` first, then the three replaceable hidden extensions.
pub fn builtin_extensions() -> Vec<BuiltinExtension> {
    vec![
        BuiltinExtension {
            name: "llama.cpp",
            replaceable: false,
            hidden: false,
            factory: || Arc::new(llama::LlamaExtension),
        },
        BuiltinExtension {
            name: "codemode",
            replaceable: true,
            hidden: true,
            factory: || Arc::new(codemode::CodemodeExtension::default()),
        },
        BuiltinExtension {
            name: "tool-search",
            replaceable: true,
            hidden: true,
            factory: || Arc::new(tool_search::ToolSearchExtension),
        },
        BuiltinExtension {
            name: "mcp",
            replaceable: true,
            hidden: true,
            factory: || Arc::new(mcp::McpExtension::default()),
        },
    ]
}

/// A `tool:`, `command:`, or `flag:` registration key, as used by
/// `omitReplacedExtensions()`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum RegistrationKey {
    Tool(String),
    Command(String),
    Flag(String),
}

impl RegistrationKey {
    pub fn tool(name: impl Into<String>) -> Self {
        Self::Tool(name.into())
    }
    pub fn command(name: impl Into<String>) -> Self {
        Self::Command(name.into())
    }
    pub fn flag(name: impl Into<String>) -> Self {
        Self::Flag(name.into())
    }

    pub fn as_str(&self) -> String {
        match self {
            Self::Tool(name) => format!("tool:{name}"),
            Self::Command(name) => format!("command:{name}"),
            Self::Flag(name) => format!("flag:{name}"),
        }
    }
}

/// A registration contributed by a user, CLI, or non-replaceable built-in extension.
#[derive(Clone, Debug)]
pub struct Registration {
    pub owner: String,
    pub replaceable: bool,
    pub key: RegistrationKey,
}

impl Registration {
    pub fn new(owner: impl Into<String>, key: RegistrationKey) -> Self {
        Self {
            owner: owner.into(),
            replaceable: false,
            key,
        }
    }

    pub fn replaceable(mut self, replaceable: bool) -> Self {
        self.replaceable = replaceable;
        self
    }
}

/// Result of resolving built-ins against user registrations.
#[derive(Clone, Debug)]
pub struct OmissionResult {
    pub active: Vec<BuiltinExtension>,
    pub omitted: Vec<String>,
    pub warnings: Vec<String>,
}

/// Built-in collision rule from `resource-loader.ts`: a non-replaceable registration of a name
/// owned by a replaceable built-in drops that built-in with a warning. Non-replaceable built-ins
/// are never omitted. `enabled` filters the candidate set by built-in name when non-empty.
pub fn omit_replaced_extensions(
    enabled: &[String],
    registrations: &[Registration],
) -> OmissionResult {
    let candidates: Vec<BuiltinExtension> = builtin_extensions()
        .into_iter()
        .filter(|builtin| enabled.is_empty() || enabled.iter().any(|name| name == builtin.name))
        .collect();
    let claimed: Vec<(&Registration, &RegistrationKey)> = registrations
        .iter()
        .filter(|registration| !registration.replaceable)
        .map(|registration| (registration, &registration.key))
        .collect();
    let mut active = Vec::new();
    let mut omitted = Vec::new();
    let mut warnings = Vec::new();
    for builtin in candidates {
        let conflict = if builtin.replaceable {
            builtin.owned_keys().into_iter().find_map(|key| {
                claimed
                    .iter()
                    .find(|(_, claimed_key)| **claimed_key == key)
                    .map(|(registration, claimed_key)| {
                        (registration.owner.clone(), claimed_key.as_str())
                    })
            })
        } else {
            None
        };
        match conflict {
            Some((owner, key)) => {
                let message = format!(
                    "Omitted replaceable built-in \"{}\": {} is registered by non-replaceable extension \"{}\"",
                    builtin.name, key, owner
                );
                warnings.push(message);
                omitted.push(builtin.name.to_string());
            }
            None => active.push(builtin),
        }
    }
    OmissionResult {
        active,
        omitted,
        warnings,
    }
}

/// Adapter factories. These names mirror upstream's `create*Extension()` exports and are the
/// integration points for the shared `Extension` trait.
pub fn create_codemode_extension(
    options: codemode::CodemodeExtensionOptions,
) -> Arc<dyn Extension> {
    Arc::new(codemode::CodemodeExtension::with_options(options))
}
pub fn create_tool_search_extension() -> Arc<dyn Extension> {
    Arc::new(tool_search::ToolSearchExtension)
}
pub fn create_mcp_extension(options: mcp::McpExtensionOptions) -> Arc<dyn Extension> {
    Arc::new(mcp::McpExtension::with_options(options))
}
pub fn llama_extension() -> Arc<dyn Extension> {
    Arc::new(llama::LlamaExtension)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_match_upstream_flags() {
        let builtins = builtin_extensions();
        let names: Vec<_> = builtins.iter().map(|builtin| builtin.name).collect();
        assert_eq!(names, ["llama.cpp", "codemode", "tool-search", "mcp"]);
        assert!(!builtins[0].replaceable);
        assert!(!builtins[0].hidden);
        assert!(
            builtins[1..]
                .iter()
                .all(|builtin| builtin.replaceable && builtin.hidden)
        );
    }

    #[test]
    fn each_builtin_instantiates_and_registers() {
        for builtin in builtin_extensions() {
            let mut registry = ExtensionRegistry::default();
            builtin.instantiate().register(&mut registry);
            let keys = registry.registration_keys();
            assert!(!keys.is_empty(), "{} registered nothing", builtin.name);
        }
    }

    #[test]
    fn non_replaceable_tool_registration_omits_codemode() {
        let result = omit_replaced_extensions(
            &[],
            &[Registration::new(
                "third-party",
                RegistrationKey::tool("codemode"),
            )],
        );
        assert_eq!(result.omitted, ["codemode"]);
        assert!(
            result
                .active
                .iter()
                .all(|builtin| builtin.name != "codemode")
        );
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("tool:codemode"));
        assert!(result.warnings[0].contains("third-party"));
    }

    #[test]
    fn non_replaceable_command_registration_omits_mcp_only() {
        let result = omit_replaced_extensions(
            &[],
            &[Registration::new("custom", RegistrationKey::command("mcp"))],
        );
        assert_eq!(result.omitted, ["mcp"]);
        assert!(
            result
                .active
                .iter()
                .any(|builtin| builtin.name == "codemode")
        );
    }

    #[test]
    fn replaceable_registrations_do_not_omit_any_builtin() {
        let registration =
            Registration::new("third-party", RegistrationKey::command("mcp")).replaceable(true);
        let result = omit_replaced_extensions(&[], &[registration]);
        assert!(result.omitted.is_empty());
        assert!(result.warnings.is_empty());
        assert_eq!(result.active.len(), 4);
    }

    #[test]
    fn permanent_llama_builtin_is_never_omitted() {
        let result = omit_replaced_extensions(
            &[],
            &[Registration::new(
                "other",
                RegistrationKey::command("llama"),
            )],
        );
        assert!(result.omitted.is_empty());
        assert!(
            result
                .active
                .iter()
                .any(|builtin| builtin.name == "llama.cpp")
        );
    }

    #[test]
    fn enabled_filters_candidates_before_collisions() {
        let result = omit_replaced_extensions(
            &["llama.cpp".to_string()],
            &[Registration::new(
                "other",
                RegistrationKey::tool("codemode"),
            )],
        );
        assert_eq!(result.active.len(), 1);
        assert_eq!(result.active[0].name, "llama.cpp");
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn registration_keys_serialize_with_kind_prefix() {
        assert_eq!(RegistrationKey::tool("read").as_str(), "tool:read");
        assert_eq!(RegistrationKey::command("mcp").as_str(), "command:mcp");
        assert_eq!(RegistrationKey::flag("verbose").as_str(), "flag:verbose");
    }
}
