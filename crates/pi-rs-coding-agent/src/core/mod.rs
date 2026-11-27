//! Core runtime of the coding agent: sessions, settings, models, compaction, extensions,
//! tools, and the agent session orchestration layer.

pub mod keybindings;
pub mod project_trust;
pub mod prompt_templates;
pub mod skills;
pub mod tools;

pub mod auth_storage;
pub mod bash_executor;
pub mod footer_data;
pub(crate) mod mcp_oauth;
pub mod messages;
pub mod model_registry;
pub mod model_resolver;
pub mod model_runtime;
pub mod resolve_config_value;
pub mod session_manager;
pub mod settings_manager;
pub mod slash_commands;
pub mod system_prompt;
pub mod timings;

pub mod compaction;
pub mod event_bus;
pub mod extensions;
pub mod resource_loader;

pub mod agent_session;
pub mod agent_session_runtime;
pub mod agent_session_services;
pub mod auth_guidance;
pub mod cache_stats;
pub mod cache_warmer;
pub mod crash_log;
pub mod http_dispatcher;
pub mod schemas;
pub mod sdk;
pub mod session_cwd;
pub mod settings_diagnostics;
pub mod usage_totals;

pub mod export_html;
pub mod nested_tool_calls;
pub mod package_manager;
pub mod startup;
