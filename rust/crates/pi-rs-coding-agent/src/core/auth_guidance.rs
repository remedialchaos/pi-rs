use std::path::{Path, PathBuf};

pub fn docs_path() -> PathBuf {
    crate::config::get_docs_path()
        .ok()
        .filter(|path| path.is_dir())
        .or_else(|| {
            let path =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../packages/coding-agent/docs");
            path.canonicalize().ok()
        })
        .unwrap_or_else(|| PathBuf::from("docs"))
}

pub fn provider_login_help() -> String {
    let docs = docs_path();
    format!(
        "Use /login to log into a provider via OAuth or API key. See:\n  {}\n  {}",
        docs.join("providers.md").display(),
        docs.join("models.md").display()
    )
}

pub fn no_models_available() -> String {
    format!("No models available. {}", provider_login_help())
}

pub fn no_api_key(provider: &str) -> String {
    let provider = if provider == "unknown" {
        "the selected model"
    } else {
        provider
    };
    format!(
        "No API key found for {provider}.\n\n{}",
        provider_login_help()
    )
}
