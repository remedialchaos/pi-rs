//! The `llama.cpp` built-in extension adapter.
//!
//! Upstream registers a native provider (`llama.cpp`), a `/llama` management command, and helpers
//! for talking to a local llama.cpp router. Unlike the other three built-ins this extension is
//! not replaceable: the provider id and command are permanent. All network work lives in the
//! `pi_rs_ai` provider; this module owns the pure configuration and formatting rules from
//! `packages/coding-agent/src/extensions/llama/{client,provider}.ts`.

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
