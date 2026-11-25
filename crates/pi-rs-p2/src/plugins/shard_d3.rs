//! Built-in native `p2` plugins owned by Wave 2 Shard D3 (`chat-mode`, `views`, `tree`, `prompt`).

use crate::api::v1::PluginRegistry;

/// Registers the Shard D3 built-in native `p2` plugins into `registry`.
pub fn register(registry: &mut PluginRegistry) -> pi_rs_chord::Result<()> {
    super::chat_mode::register(registry)?;
    super::views::register(registry)?;
    super::tree::register(registry)?;
    super::prompt::register(registry)?;
    Ok(())
}
