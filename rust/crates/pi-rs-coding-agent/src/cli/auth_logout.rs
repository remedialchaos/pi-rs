//! Remove the stored login, leaving environment and model configuration alone.

use std::path::Path;

use pi_rs_ai::auth::AuthType;

use super::auth_command::{Runtime, deadline, find_provider};
use crate::utils::web_url::replace_control_characters;

pub(super) async fn run(args: &[&str], agent_dir: &Path) -> u8 {
    let [provider_ref] = args else {
        eprintln!("Usage: pi auth logout <provider>");
        return 1;
    };
    if provider_ref.starts_with('-') {
        eprintln!("Usage: pi auth logout <provider>");
        return 1;
    }
    let signal = deadline();
    let runtime = match Runtime::new(agent_dir, false, false) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Could not read stored credentials: {error}");
            return 1;
        }
    };
    let stored = match runtime.credentials.list(&signal).await {
        Ok(stored) => stored,
        Err(error) => {
            eprintln!("Could not read stored credentials: {error}");
            return 1;
        }
    };
    let providers = runtime.models().get_providers();
    let provider = find_provider(&providers, provider_ref);
    // Providers supplied by extensions may not be loaded; their stored ids still work.
    let id = provider.map_or(*provider_ref, |provider| provider.id.as_str());
    let credential = stored
        .iter()
        .find(|credential| credential.provider_id == id);
    if provider.is_none() && credential.is_none() {
        eprintln!(
            "Unknown provider \"{}\".",
            replace_control_characters(provider_ref)
        );
        let mut ids: Vec<_> = stored
            .iter()
            .map(|item| item.provider_id.as_str())
            .collect();
        ids.sort_unstable();
        let ids = if ids.is_empty() {
            "none".into()
        } else {
            ids.join(", ")
        };
        eprintln!("Stored credentials: {ids}");
        return 1;
    }
    let name = provider.map_or(*provider_ref, |provider| provider.name.as_str());
    let Some(credential) = credential else {
        println!(
            "No stored credentials for {name}. Environment variables and models.json config are unchanged."
        );
        return 0;
    };
    if let Err(error) = runtime.models().logout(id, &signal).await {
        eprintln!("Logout failed: {error}");
        return 1;
    }
    if let Err(error) = runtime.synchronize(id, &signal).await {
        eprintln!(
            "Credentials removed for {name}, but local model state could not be synchronized: {error}"
        );
        return 1;
    }
    if credential.auth_type == AuthType::OAuth {
        println!("Logged out of {name}.");
    } else {
        println!(
            "Removed stored API key for {name}. Environment variables and models.json config are unchanged."
        );
    }
    0
}
