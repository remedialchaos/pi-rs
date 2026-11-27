use std::sync::{Arc, LazyLock, RwLock};

use indexmap::IndexMap;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::event_stream::AssistantMessageEventStream;
use crate::model::{Api, ImageApi, ImageModel, Model};
use crate::openrouter_images::ImagesOptions;
use crate::providers::{builtin_provider_specs, materialize};
use crate::registry::{ImageGenerator, Models, ModelsOptions};
use crate::request::{ProviderEnv, StreamOptions, provider_env_value};
use crate::transcript::normalize_context;
use crate::types::{AssistantImages, Context, FinalMessage, ImagesContext, TranscriptContext};

pub type ApiStream = Arc<
    dyn Fn(Model, TranscriptContext, StreamOptions, bool) -> Result<AssistantMessageEventStream>
        + Send
        + Sync,
>;
#[derive(Clone)]
pub struct ApiProvider {
    pub api: Api,
    pub stream: ApiStream,
}
struct Entry {
    adapter: ApiProvider,
    source: Option<String>,
    builtin: bool,
}
const APIS: [Api; 10] = [
    Api::AnthropicMessages,
    Api::OpenAiCompletions,
    Api::OpenAiResponses,
    Api::OpenAiCodexResponses,
    Api::AzureOpenAiResponses,
    Api::GoogleGenerativeAi,
    Api::GoogleVertex,
    Api::MistralConversations,
    Api::BedrockConverseStream,
    Api::PiMessages,
];
static REGISTRY: LazyLock<RwLock<IndexMap<Api, Entry>>> = LazyLock::new(|| {
    let mut entries = IndexMap::new();
    register_builtins(&mut entries);
    RwLock::new(entries)
});
fn register_builtins(entries: &mut IndexMap<Api, Entry>) {
    for api in APIS {
        entries.entry(api.clone()).or_insert_with(|| Entry {
            adapter: ApiProvider {
                api,
                stream: Arc::new(crate::registry::dispatch),
            },
            source: None,
            builtin: true,
        });
    }
}
pub fn register_api_provider(adapter: ApiProvider, source: Option<String>) {
    let expected = adapter.api.clone();
    let stream = adapter.stream;
    let guard = expected.clone();
    let stream = Arc::new(move |model: Model, context, opts, simple| {
        if model.api != guard {
            return Err(Error::Protocol(format!(
                "Mismatched api: {} expected {guard}",
                model.api
            )));
        }
        stream(model, context, opts, simple)
    });
    REGISTRY.write().unwrap_or_else(|e| e.into_inner()).insert(
        expected,
        Entry {
            adapter: ApiProvider {
                api: adapter.api,
                stream,
            },
            source,
            builtin: false,
        },
    );
}
pub fn get_api_provider(api: &Api) -> Option<ApiProvider> {
    REGISTRY
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(api)
        .map(|entry| entry.adapter.clone())
}
pub fn get_api_providers() -> Vec<ApiProvider> {
    REGISTRY
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .map(|e| e.adapter.clone())
        .collect()
}
pub fn unregister_api_providers(source: &str) {
    REGISTRY
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, e| e.source.as_deref() != Some(source));
}
pub fn register_builtin_api_providers() {
    register_builtins(&mut REGISTRY.write().unwrap_or_else(|e| e.into_inner()));
}
pub fn reset_api_providers() {
    let mut r = REGISTRY.write().unwrap_or_else(|e| e.into_inner());
    r.clear();
    register_builtins(&mut r);
}
pub fn get_model(provider: &str, id: &str) -> Result<Option<&'static Model>> {
    Ok(Catalog::embedded()?.get_model(provider, id))
}
pub fn get_models(provider: &str) -> Result<Vec<&'static Model>> {
    Ok(Catalog::embedded()?
        .chat_models(provider)
        .into_iter()
        .flat_map(|m| m.values())
        .collect())
}
pub fn get_providers() -> Result<Vec<&'static str>> {
    Ok(Catalog::embedded()?.providers().collect())
}
pub fn get_image_model(provider: &str, id: &str) -> Result<Option<&'static ImageModel>> {
    Ok(Catalog::embedded()?.get_image_model(provider, id))
}
pub fn get_image_models(provider: &str) -> Result<Vec<&'static ImageModel>> {
    Ok(Catalog::embedded()?
        .image_models(provider)
        .into_iter()
        .flat_map(|m| m.values())
        .collect())
}
pub fn get_image_providers() -> Result<Vec<&'static str>> {
    Ok(Catalog::embedded()?.image_providers().collect())
}

fn env_names(provider: &str) -> Result<&'static [String]> {
    let Some(s) = builtin_provider_specs()?.iter().find(|p| p.id == provider) else {
        return Ok(&[]);
    };
    Ok(match provider {
        "amazon-bedrock" => &[],
        "anthropic" => &s.env[..s.env.len().min(3)],
        "google-vertex" | "cloudflare-workers-ai" | "cloudflare-ai-gateway" => {
            &s.env[..s.env.len().min(1)]
        }
        _ => &s.env,
    })
}
pub fn find_env_keys(provider: &str, env: &ProviderEnv) -> Result<Vec<&'static str>> {
    Ok(env_names(provider)?
        .iter()
        .filter(|key| provider_env_value(key, env).is_some())
        .map(String::as_str)
        .collect())
}
pub fn get_env_api_key(provider: &str, env: &ProviderEnv) -> Result<Option<String>> {
    for name in env_names(provider)? {
        if provider == "anthropic" && name == "ANTHROPIC_AUTH_TOKEN" {
            continue;
        }
        if let Some(key) = provider_env_value(name, env) {
            return Ok(Some(key.into_owned()));
        }
    }
    let has = |key| provider_env_value(key, env).is_some();
    if provider == "amazon-bedrock"
        && ((has("AWS_ACCESS_KEY_ID") && has("AWS_SECRET_ACCESS_KEY"))
            || [
                "AWS_PROFILE",
                "AWS_BEARER_TOKEN_BEDROCK",
                "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
                "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                "AWS_WEB_IDENTITY_TOKEN_FILE",
            ]
            .into_iter()
            .any(has))
        || provider == "google-vertex"
            && (has("GOOGLE_CLOUD_PROJECT") || has("GCLOUD_PROJECT"))
            && has("GOOGLE_CLOUD_LOCATION")
            && crate::auth::home_path(
                provider_env_value("GOOGLE_APPLICATION_CREDENTIALS", env)
                    .as_deref()
                    .unwrap_or("~/.config/gcloud/application_default_credentials.json"),
            )
            .exists()
    {
        return Ok(Some("<authenticated>".into()));
    }
    Ok(None)
}
/// The built-in providers, resolved through `Models` so stored `auth.json` credentials,
/// OAuth refresh, Bedrock/Vertex ambient auth and the `x-opencode-session` header apply.
static BUILTIN_MODELS: LazyLock<std::result::Result<Models, String>> = LazyLock::new(|| {
    Models::builtin(ModelsOptions {
        credentials: Arc::new(crate::auth::default_credential_store()),
        ..ModelsOptions::default()
    })
    .map_err(|e| e.to_string())
});
fn builtin_models() -> Result<&'static Models> {
    BUILTIN_MODELS
        .as_ref()
        .map_err(|e| Error::ModelSource(e.clone()))
}

/// True when the model belongs to a built-in catalog provider and that provider serves the
/// model's API, like upstream's `getBuiltinProviderForModel`.
fn is_builtin_provider_model(model: &Model) -> bool {
    Catalog::embedded().is_ok_and(|catalog| {
        catalog
            .chat_models(&model.provider)
            .is_some_and(|models| models.values().any(|entry| entry.api == model.api))
    })
}
fn dispatch(
    mut model: Model,
    context: Context,
    mut opts: StreamOptions,
    simple: bool,
) -> Result<AssistantMessageEventStream> {
    let (adapter, builtin) = {
        let registry = REGISTRY.read().unwrap_or_else(|e| e.into_inner());
        let entry = registry.get(&model.api).ok_or_else(|| {
            Error::Protocol(format!("No API provider registered for api: {}", model.api))
        })?;
        (entry.adapter.stream.clone(), entry.builtin)
    };
    let explicit = opts
        .request
        .api_key
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty());
    // Cloudflare keeps its direct path when the caller already resolved its binding
    // authorization, like upstream's `hasResolvedCloudflareAuth`.
    let cloudflare_resolved = explicit
        || opts.request.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("cf-aig-authorization") && value.is_some()
        });
    if builtin
        && !(model.provider.starts_with("cloudflare-") && cloudflare_resolved)
        && is_builtin_provider_model(&model)
    {
        // Built-in providers resolve their provider through `Models`, so stored
        // `auth.json` credentials, OAuth refresh, Bedrock/Vertex ambient auth and the
        // `x-opencode-session` header apply to every one of them.
        let models = builtin_models()?;
        return Ok(if simple {
            models.stream_simple(model, context, opts)
        } else {
            models.stream(model, context, opts)
        });
    }
    if !explicit
        && let Some(key) =
            get_env_api_key(&model.provider, &opts.request.env)?.filter(|s| s != "<authenticated>")
    {
        opts.request.api_key = Some(key);
    }
    if builtin {
        materialize(&mut model.base, &opts.request.env);
        crate::registry::azure_chat(&mut model, &mut opts)?;
    }
    adapter(model, normalize_context(context), opts, simple)
}
pub fn stream(
    model: Model,
    context: Context,
    opts: StreamOptions,
) -> Result<AssistantMessageEventStream> {
    dispatch(model, context, opts, false)
}
pub fn stream_simple(
    model: Model,
    context: Context,
    opts: StreamOptions,
) -> Result<AssistantMessageEventStream> {
    dispatch(model, context, opts, true)
}
pub async fn complete(model: Model, context: Context, opts: StreamOptions) -> Result<FinalMessage> {
    stream(model, context, opts)?.result().await
}
pub async fn complete_simple(
    model: Model,
    context: Context,
    opts: StreamOptions,
) -> Result<FinalMessage> {
    stream_simple(model, context, opts)?.result().await
}

static IMAGE_APIS: LazyLock<RwLock<IndexMap<ImageApi, ImageGenerator>>> = LazyLock::new(|| {
    let generate: ImageGenerator = Arc::new(|model, context, options| {
        Box::pin(crate::openrouter_images::generate_images(
            model, context, options,
        ))
    });
    RwLock::new(IndexMap::from([(ImageApi::OpenRouterImages, generate)]))
});

pub fn register_images_api_provider(api: ImageApi, generate: ImageGenerator) {
    IMAGE_APIS
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(api, generate);
}

pub fn get_images_api_provider(api: &ImageApi) -> Option<ImageGenerator> {
    IMAGE_APIS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(api)
        .cloned()
}

pub async fn generate_images(
    model: &ImageModel,
    context: &ImagesContext,
    options: &ImagesOptions,
) -> Result<AssistantImages> {
    let generate = get_images_api_provider(&model.api).ok_or_else(|| {
        Error::Protocol(format!("No API provider registered for api: {}", model.api))
    })?;
    Ok(generate(model, context, options).await)
}

pub type SessionResourceCleanup = Arc<dyn Fn(Option<&str>) + Send + Sync>;
static SESSION_CLEANUPS: LazyLock<RwLock<IndexMap<u64, SessionResourceCleanup>>> =
    LazyLock::new(|| RwLock::new(IndexMap::new()));

pub struct SessionCleanupRegistration(u64);
impl Drop for SessionCleanupRegistration {
    fn drop(&mut self) {
        SESSION_CLEANUPS
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .shift_remove(&self.0);
    }
}
pub fn register_session_resource_cleanup(
    cleanup: SessionResourceCleanup,
) -> SessionCleanupRegistration {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    SESSION_CLEANUPS
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, cleanup);
    SessionCleanupRegistration(id)
}

pub fn cleanup_session_resources(session: Option<&str>) {
    crate::anthropic::close_sessions(session);
    crate::codex::close_sessions(session);
    let registered = SESSION_CLEANUPS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for cleanup in registered {
        cleanup(session);
    }
}

pub use crate::anthropic::{stream as stream_anthropic, stream_simple as stream_simple_anthropic};
pub use crate::codex::{
    stream as stream_openai_codex_responses, stream_simple as stream_simple_openai_codex_responses,
};
pub use crate::google::{
    stream as stream_google, stream as stream_google_vertex, stream_simple as stream_simple_google,
    stream_simple as stream_simple_google_vertex,
};
pub use crate::mistral::{stream as stream_mistral, stream_simple as stream_simple_mistral};
pub use crate::openai_completions::{
    stream as stream_openai_completions, stream_simple as stream_simple_openai_completions,
};
pub use crate::responses::{
    stream as stream_azure_openai_responses, stream as stream_openai_responses,
    stream_simple as stream_simple_azure_openai_responses,
    stream_simple as stream_simple_openai_responses,
};
