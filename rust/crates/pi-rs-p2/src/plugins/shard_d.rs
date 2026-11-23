//! Wave 2 Plugin Shard D registration (22 plugins owned by coordinator; D2 and D3 own the remaining 9).

use crate::api::v1::PluginRegistry;

use super::{
    agent_plugins, cache_warming, codemode, coding_tools, dev_tools, devices, drafts, file_index,
    files, fit_images, logs, powershell, projects, proxy_marker, quiet_tools, scope_prompt,
    send_file, settings, status, telemetry, web_shell,
};

pub fn register(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    proxy_marker::register(registry)?;
    drafts::register(registry)?;
    logs::register(registry)?;
    file_index::register(registry)?;
    devices::register(registry)?;
    projects::register(registry)?;
    dev_tools::register(registry)?;
    settings::register(registry)?;
    status::register(registry)?;
    fit_images::register(registry)?;
    cache_warming::register(registry)?;
    scope_prompt::register(registry)?;
    agent_plugins::register(registry)?;
    quiet_tools::register(registry)?;
    web_shell::register(registry)?;
    send_file::register(registry)?;
    powershell::register(registry)?;
    coding_tools::register(registry)?;
    files::register(registry)?;
    codemode::register(registry)?;
    telemetry::register(registry)?;
    Ok(())
}
