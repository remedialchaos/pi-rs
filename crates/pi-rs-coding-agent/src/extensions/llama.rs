//! The `llama.cpp` built-in extension adapter.
//!
//! Upstream registers a native provider (`llama.cpp`), a `/llama` management command, and helpers
//! for talking to a local llama.cpp router. Unlike the other three built-ins this extension is
//! not replaceable: the provider id and command are permanent. All network work lives in the
//! `pi_rs_ai` provider; this module owns the pure configuration and formatting rules from
//! `packages/coding-agent/src/extensions/llama/{client,provider}.ts`.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::core::auth_storage::AuthStorage;
use crate::core::extensions::types::{ExtensionCommandContext, ProviderRegistration};

use super::{Extension, ExtensionRegistry};

pub const LLAMA_PROVIDER_ID: &str = "llama.cpp";
pub const DEFAULT_LLAMA_SERVER_URL: &str = "http://127.0.0.1:9931";
pub const LLAMA_COMMAND: &str = "llama";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LlamaError {
    InvalidUrl(String),
    UnsupportedScheme(String),
}

impl std::fmt::Display for LlamaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(message) => write!(formatter, "Invalid server URL: {message}"),
            Self::UnsupportedScheme(scheme) => {
                write!(formatter, "Server URL must use http or https, got {scheme}")
            }
        }
    }
}

impl std::error::Error for LlamaError {}

/// Normalize a configured llama.cpp router URL. Fragments, queries, trailing slashes, and a
/// trailing `/v1` are removed; only http and https are accepted.
pub fn normalize_llama_server_url(value: &str) -> Result<String, LlamaError> {
    let mut url =
        url::Url::parse(value.trim()).map_err(|error| LlamaError::InvalidUrl(error.to_string()))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(LlamaError::UnsupportedScheme(url.scheme().to_string()));
    }
    url.set_fragment(None);
    url.set_query(None);
    let path = url
        .path()
        .trim_end_matches('/')
        .strip_suffix("/v1")
        .unwrap_or_else(|| url.path().trim_end_matches('/'))
        .to_string();
    url.set_path(&path);
    Ok(url.to_string().trim_end_matches('/').to_string())
}

/// Inference endpoint of a normalized server URL.
pub fn llama_inference_url(server_url: &str) -> Result<String, LlamaError> {
    Ok(format!("{}/v1", normalize_llama_server_url(server_url)?))
}

/// Format a byte count like upstream's `formatBytes` (KiB/MiB/GiB/TiB, two decimals below 10).
pub fn format_bytes(bytes: f64) -> String {
    if bytes < 1024.0 {
        return format!("{bytes} B");
    }
    let units = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes / 1024.0;
    let mut unit = units[0];
    for next in units.iter().skip(1) {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    let formatted = if value >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    };
    format!("{formatted} {unit}")
}

/// Split a Hugging Face `repository:quantization` identifier.
pub fn parse_hugging_face_model(value: &str) -> (String, Option<String>) {
    let search_from = value.find('/').map_or(0, |position| position + 1);
    match value[search_from..].find(':') {
        Some(offset) => {
            let colon = search_from + offset;
            (
                value[..colon].to_string(),
                Some(value[colon + 1..].to_string()),
            )
        }
        None => (value.to_string(), None),
    }
}

/// A model is usable when it is loaded or sleeping (requests wake sleeping models).
pub fn model_is_loaded(status: &str) -> bool {
    matches!(status, "loaded" | "sleeping")
}

/// Network failures get a stable message, matching upstream's user-facing behavior.
pub fn connection_error_message(message: &str) -> &'static str {
    let lowered = message.to_lowercase();
    if lowered.contains("fetch failed")
        || lowered.contains("timeout")
        || lowered.contains("network")
    {
        "Could not connect to the server."
    } else {
        "An unexpected error occurred."
    }
}

/// The permanent `llama.cpp` extension.
#[derive(Clone, Copy, Debug, Default)]
pub struct LlamaExtension;

impl Extension for LlamaExtension {
    fn name(&self) -> &'static str {
        "llama.cpp"
    }

    fn register(&self, registry: &mut ExtensionRegistry) {
        registry.register_provider(LLAMA_PROVIDER_ID);
        registry.register_command(LLAMA_COMMAND);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlamaModelStatus {
    pub value: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub failed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlamaModelMeta {
    pub n_ctx: Option<u64>,
    pub n_ctx_train: Option<u64>,
    pub size: Option<u64>,
    pub ftype: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlamaModelInfo {
    pub id: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub status: LlamaModelStatus,
    pub meta: Option<LlamaModelMeta>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlamaServerProps {
    #[serde(default)]
    pub models_autoload: bool,
    pub chat_template: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LlamaClient {
    pub server_url: String,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl LlamaClient {
    pub fn new(server_url: &str, api_key: Option<String>) -> Result<Self, LlamaError> {
        Ok(Self {
            server_url: normalize_llama_server_url(server_url)?,
            api_key: api_key.filter(|k| !k.trim().is_empty()),
            http: reqwest::Client::new(),
        })
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => req.bearer_auth(key),
            None => req,
        }
    }

    pub async fn list(&self) -> Result<Vec<LlamaModelInfo>, String> {
        let url = format!("{}/models", self.server_url);
        let resp = self
            .apply_auth(self.http.get(&url))
            .send()
            .await
            .map_err(|error| connection_error_message(&error.to_string()).to_owned())?;
        if !resp.status().is_success() {
            return Err(format!("llama.cpp /models returned {}", resp.status()));
        }
        let body: Value = resp.json().await.map_err(|error| error.to_string())?;
        serde_json::from_value(body["data"].clone()).map_err(|error| error.to_string())
    }

    pub async fn props(&self) -> Result<LlamaServerProps, String> {
        let url = format!("{}/props", self.server_url);
        let resp = self
            .apply_auth(self.http.get(&url))
            .send()
            .await
            .map_err(|error| connection_error_message(&error.to_string()).to_owned())?;
        if !resp.status().is_success() {
            return Err(format!("llama.cpp /props returned {}", resp.status()));
        }
        resp.json().await.map_err(|error| error.to_string())
    }

    pub async fn load(&self, model: &str) -> Result<(), String> {
        let url = format!("{}/models/load", self.server_url);
        let resp = self
            .apply_auth(self.http.post(&url).json(&json!({ "model": model })))
            .send()
            .await
            .map_err(|error| connection_error_message(&error.to_string()).to_owned())?;
        if !resp.status().is_success() {
            return Err(format!("llama.cpp load failed ({})", resp.status()));
        }
        Ok(())
    }

    pub async fn unload(&self, model: &str) -> Result<(), String> {
        let url = format!("{}/models/unload", self.server_url);
        let resp = self
            .apply_auth(self.http.post(&url).json(&json!({ "model": model })))
            .send()
            .await
            .map_err(|error| connection_error_message(&error.to_string()).to_owned())?;
        if !resp.status().is_success() {
            return Err(format!("llama.cpp unload failed ({})", resp.status()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HuggingFaceModel {
    pub id: String,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub likes: u64,
}

#[derive(Clone, Debug)]
pub struct HuggingFaceClient {
    base_url: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Default for HuggingFaceClient {
    fn default() -> Self {
        Self::new(
            std::env::var("HF_TOKEN")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            "https://huggingface.co",
        )
    }
}

impl HuggingFaceClient {
    pub fn new(token: Option<String>, base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            token,
            http: reqwest::Client::new(),
        }
    }

    pub async fn search(&self, query: &str) -> Result<Vec<HuggingFaceModel>, String> {
        let mut req = self
            .http
            .get(format!("{}/api/models", self.base_url))
            .query(&[
                ("search", query),
                ("filter", "gguf"),
                ("sort", "downloads"),
                ("direction", "-1"),
                ("limit", "25"),
            ]);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(|error| error.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("Hugging Face search returned {}", resp.status()));
        }
        resp.json().await.map_err(|error| error.to_string())
    }
}

pub fn provider_registration(extension_path: &str) -> ProviderRegistration {
    let server_url = std::env::var("LLAMA_BASE_URL")
        .ok()
        .and_then(|url| normalize_llama_server_url(&url).ok())
        .unwrap_or_else(|| DEFAULT_LLAMA_SERVER_URL.to_owned());
    let base_url = format!("{server_url}/v1");
    ProviderRegistration {
        name: LLAMA_PROVIDER_ID.to_owned(),
        config: json!({
            "baseUrl": base_url,
            "api": "openai-completions",
            "apiKey": "llama-cpp",
            "models": [],
        }),
        extension_path: extension_path.to_owned(),
    }
}

pub async fn run_llama_command(agent_dir: &Path, args: &str, context: &ExtensionCommandContext) {
    if !context.base.ui.has_ui {
        context
            .base
            .ui
            .notify("/llama is available in interactive mode", "warning");
        return;
    }
    let storage = AuthStorage::create(agent_dir.join("auth.json"));
    let configured_url = std::env::var("LLAMA_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            storage
                .list()
                .into_iter()
                .find(|entry| entry.provider_id == LLAMA_PROVIDER_ID)
                .map(|_| DEFAULT_LLAMA_SERVER_URL.to_owned())
        });
    let server_url = match configured_url {
        Some(url) => url,
        None => match context
            .base
            .ui
            .input_with_options(
                "llama.cpp server URL",
                Some(DEFAULT_LLAMA_SERVER_URL.to_owned()),
                Default::default(),
            )
            .await
        {
            Some(url) if !url.trim().is_empty() => url,
            _ => {
                context
                    .base
                    .ui
                    .notify("Configure llama.cpp with /login llama.cpp", "warning");
                return;
            }
        },
    };
    let client = match LlamaClient::new(&server_url, None) {
        Ok(client) => client,
        Err(error) => {
            context.base.ui.notify(error.to_string(), "error");
            return;
        }
    };
    let trimmed = args.trim();
    if let Some(repo) = trimmed.strip_prefix("download ") {
        let (repository, quant) = parse_hugging_face_model(repo.trim());
        let hf = HuggingFaceClient::default();
        match hf.search(&repository).await {
            Ok(models) if !models.is_empty() => {
                let target =
                    quant.map_or_else(|| models[0].id.clone(), |q| format!("{}:{q}", models[0].id));
                match client.load(&target).await {
                    Ok(()) => context
                        .base
                        .ui
                        .notify(format!("Load started for {target}"), "info"),
                    Err(error) => context.base.ui.notify(error, "error"),
                }
            }
            Ok(_) => context.base.ui.notify(
                format!("No Hugging Face models found for {repository}"),
                "warning",
            ),
            Err(error) => context.base.ui.notify(error, "error"),
        }
        return;
    }
    let catalog = match client.list().await {
        Ok(models) => models,
        Err(error) => {
            context.base.ui.notify(error, "error");
            return;
        }
    };
    let mut items: Vec<String> = catalog
        .iter()
        .map(|model| format!("{} [{}]", model.id, model.status.value))
        .collect();
    items.push("Download model from Hugging Face…".to_owned());
    let Some(picked) = context
        .base
        .ui
        .select_with_options(
            format!("llama.cpp ({})", client.server_url),
            items,
            Default::default(),
        )
        .await
    else {
        return;
    };
    if picked == "Download model from Hugging Face…" {
        let Some(query) = context
            .base
            .ui
            .input_with_options("Search Hugging Face GGUF models", None, Default::default())
            .await
        else {
            return;
        };
        let hf = HuggingFaceClient::default();
        match hf.search(query.trim()).await {
            Ok(results) if !results.is_empty() => {
                let choices: Vec<String> = results.into_iter().map(|m| m.id).collect();
                if let Some(repo) = context
                    .base
                    .ui
                    .select_with_options("Select GGUF model to load", choices, Default::default())
                    .await
                {
                    match client.load(&repo).await {
                        Ok(()) => context
                            .base
                            .ui
                            .notify(format!("Load started for {repo}"), "info"),
                        Err(error) => context.base.ui.notify(error, "error"),
                    }
                }
            }
            Ok(_) => context
                .base
                .ui
                .notify("No matching GGUF models found.", "warning"),
            Err(error) => context.base.ui.notify(error, "error"),
        }
        return;
    }
    let model_id = picked.split(" [").next().unwrap_or(&picked).to_owned();
    let is_loaded = catalog
        .iter()
        .find(|m| m.id == model_id)
        .is_some_and(|m| model_is_loaded(&m.status.value));
    if is_loaded {
        if context
            .base
            .ui
            .confirm_with_options("Unload model?", &model_id, Default::default())
            .await
        {
            match client.unload(&model_id).await {
                Ok(()) => context
                    .base
                    .ui
                    .notify(format!("Unloaded {model_id}"), "info"),
                Err(error) => context.base.ui.notify(error, "error"),
            }
        }
    } else {
        match client.load(&model_id).await {
            Ok(()) => context.base.ui.notify(format!("Loaded {model_id}"), "info"),
            Err(error) => context.base.ui.notify(error, "error"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_router_urls() {
        assert_eq!(
            normalize_llama_server_url(" http://127.0.0.1:9931/v1/ ").expect("valid URL"),
            "http://127.0.0.1:9931"
        );
        assert_eq!(
            normalize_llama_server_url("https://example.com/router?token=x#frag")
                .expect("valid URL"),
            "https://example.com/router"
        );
    }

    #[test]
    fn rejects_non_http_schemes() {
        assert_eq!(
            normalize_llama_server_url("ftp://example.com"),
            Err(LlamaError::UnsupportedScheme("ftp".to_string()))
        );
        assert!(matches!(
            normalize_llama_server_url("not a url"),
            Err(LlamaError::InvalidUrl(_))
        ));
    }

    #[test]
    fn inference_url_appends_v1() {
        assert_eq!(
            llama_inference_url(DEFAULT_LLAMA_SERVER_URL).expect("default URL"),
            "http://127.0.0.1:9931/v1"
        );
    }

    #[test]
    fn formats_bytes_like_upstream() {
        assert_eq!(format_bytes(512.0), "512 B");
        assert_eq!(format_bytes(1536.0), "1.50 KiB");
        assert_eq!(format_bytes(20.0 * 1024.0 * 1024.0), "20.0 MiB");
    }

    #[test]
    fn parses_hugging_face_quantization() {
        assert_eq!(
            parse_hugging_face_model("org/model:Q4_K_M"),
            ("org/model".to_string(), Some("Q4_K_M".to_string()))
        );
        assert_eq!(
            parse_hugging_face_model("org/model"),
            ("org/model".to_string(), None)
        );
    }

    #[test]
    fn sleeping_models_are_loaded() {
        assert!(model_is_loaded("loaded"));
        assert!(model_is_loaded("sleeping"));
        assert!(!model_is_loaded("unloaded"));
    }

    #[test]
    fn connection_errors_are_classified() {
        assert_eq!(
            connection_error_message("fetch failed"),
            "Could not connect to the server."
        );
        assert_eq!(
            connection_error_message("boom"),
            "An unexpected error occurred."
        );
    }

    #[test]
    fn extension_registers_permanent_provider_and_command() {
        let mut registry = ExtensionRegistry::default();
        LlamaExtension.register(&mut registry);
        assert_eq!(registry.providers, [LLAMA_PROVIDER_ID]);
        assert_eq!(registry.commands, [LLAMA_COMMAND]);
    }
}
