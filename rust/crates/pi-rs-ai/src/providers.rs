use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::auth::{
    ApiKeyAuth, ApiKeyCredential, AuthContext, AuthEvent, AuthInteraction, AuthPrompt, AuthResult,
    LoginOptions, ModelAuth, PromptKind, ProviderAuth, SelectOption,
};
use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::http::abortable;
use crate::model::{
    AnyModel, Api, BaseModel, ClassifierApi, ImageApi, Model, ModelCost, ModelCostRates,
    ModelCostTier,
};
use crate::oauth::{BuiltinOAuth, Flow};
use crate::registry::{ModelSource, ModelSourceRequest, Provider};
use crate::request::{ProviderEnv, ProviderHeaders};
use crate::types::unix_millis;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltinProviderSpec {
    pub id: String,
    pub name: String,
    pub key: Option<String>,
    pub env: Vec<String>,
    pub dynamic: bool,
    pub base_url: Option<String>,
    pub headers: Option<IndexMap<String, String>>,
}
impl BuiltinProviderSpec {
    pub fn oauth_flow(&self) -> Option<Flow> {
        match self.id.as_str() {
            "anthropic" => Some(Flow::Anthropic),
            "github-copilot" => Some(Flow::Copilot),
            "kimi-coding" => Some(Flow::Kimi),
            "meta" => Some(Flow::Meta),
            "openai" => Some(Flow::ChatGpt),
            "openai-codex" => Some(Flow::Codex),
            "openrouter" => Some(Flow::OpenRouter),
            "radius" => Some(Flow::Radius),
            "xai" => Some(Flow::Xai),
            _ => None,
        }
    }
    fn install(&self, catalog: &Catalog) -> Result<Provider> {
        let mut provider = Provider::new(self.id.clone(), self.name.clone());
        provider.base_url.clone_from(&self.base_url);
        provider.headers.clone_from(&self.headers);
        provider.auth = ProviderAuth {
            api_key: self.key.as_ref().map(|name| {
                Arc::new(KeyAuth {
                    name: name.clone(),
                    keys: self.env.clone(),
                    kind: KeyKind::from(self.id.as_str()),
                }) as _
            }),
            oauth: self
                .oauth_flow()
                .map(BuiltinOAuth::new)
                .transpose()?
                .map(|oauth| Arc::new(oauth) as _),
        };
        provider.models = catalog
            .all_provider_models(&self.id)
            .collect::<Vec<_>>()
            .into();
        for model in provider.models.iter() {
            match model {
                AnyModel::Chat(m) => {
                    provider.chat_apis.insert(m.api.clone());
                }
                AnyModel::Image(m) => {
                    provider.image_apis.insert(m.api.clone());
                }
                AnyModel::Classifier(m) => {
                    provider.classifier_apis.insert(m.api.clone());
                }
            }
        }
        match self.id.as_str() {
            "cloudflare-ai-gateway" | "github-copilot" => {
                provider.chat_apis.extend([
                    Api::AnthropicMessages,
                    Api::OpenAiCompletions,
                    Api::OpenAiResponses,
                ]);
            }
            "cloudflare-workers-ai" => {
                provider
                    .classifier_apis
                    .insert(ClassifierApi::CloudflareWorkersAiSystemOne);
            }
            "openai" => {
                provider
                    .classifier_apis
                    .insert(ClassifierApi::OpenAiDecisions);
            }
            "openrouter" => {
                provider.image_apis.insert(ImageApi::OpenRouterImages);
                provider
                    .classifier_apis
                    .insert(ClassifierApi::TypeSafeSystemOne);
            }
            "radius" => {
                provider.chat_apis.insert(Api::PiMessages);
                provider.source = Some(Arc::new(RadiusSource {
                    gateway: Url::parse("https://radius.pi.dev")?,
                }));
            }
            _ => {}
        }
        Ok(provider)
    }
}
pub fn builtin_provider_specs() -> Result<&'static [BuiltinProviderSpec]> {
    static SPECS: OnceLock<std::result::Result<Vec<BuiltinProviderSpec>, String>> = OnceLock::new();
    SPECS
        .get_or_init(|| {
            serde_json::from_str(include_str!("../assets/providers.json"))
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(|e| Error::ModelSource(e.clone()))
}
pub fn builtin_providers(catalog: &Catalog) -> Result<Vec<Provider>> {
    builtin_provider_specs()?
        .iter()
        .map(|p| p.install(catalog))
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyKind {
    Generic,
    Keyless,
    Anthropic,
    Bedrock,
    Vertex,
    CloudflareWorkers,
    CloudflareGateway,
}
impl From<&str> for KeyKind {
    fn from(id: &str) -> Self {
        match id {
            "anthropic" => Self::Anthropic,
            "amazon-bedrock" => Self::Bedrock,
            "google-vertex" => Self::Vertex,
            "cloudflare-workers-ai" => Self::CloudflareWorkers,
            "cloudflare-ai-gateway" => Self::CloudflareGateway,
            _ => Self::Generic,
        }
    }
}
struct KeyAuth {
    name: String,
    keys: Vec<String>,
    kind: KeyKind,
}
pub fn env_api_key_auth(
    name: impl Into<String>,
    keys: impl IntoIterator<Item = impl Into<String>>,
) -> Arc<dyn ApiKeyAuth> {
    Arc::new(KeyAuth {
        name: name.into(),
        keys: keys.into_iter().map(Into::into).collect(),
        kind: KeyKind::Generic,
    })
}
pub fn keyless_auth() -> Arc<dyn ApiKeyAuth> {
    Arc::new(KeyAuth {
        name: "No API key required".into(),
        keys: vec![],
        kind: KeyKind::Keyless,
    })
}
fn resolved(key: Option<String>, env: ProviderEnv, source: &str) -> AuthResult {
    AuthResult {
        auth: ModelAuth {
            api_key: key,
            ..ModelAuth::default()
        },
        env,
        source: Some(source.into()),
    }
}
async fn ask(
    ui: &dyn AuthInteraction,
    opts: &LoginOptions,
    message: &str,
    kind: PromptKind,
) -> Result<String> {
    let signal = opts.signal.clone();
    abortable(
        &signal,
        ui.prompt(AuthPrompt {
            kind,
            message: message.into(),
            placeholder: None,
            signal: signal.clone(),
        }),
    )
    .await
}
async fn choose(
    ui: &dyn AuthInteraction,
    opts: &LoginOptions,
    message: &str,
    choices: &[(&str, &str)],
) -> Result<String> {
    ask(
        ui,
        opts,
        message,
        PromptKind::Select(
            choices
                .iter()
                .map(|(id, label)| SelectOption {
                    id: (*id).into(),
                    label: (*label).into(),
                    description: None,
                })
                .collect(),
        ),
    )
    .await
}
#[async_trait]
impl ApiKeyAuth for KeyAuth {
    fn name(&self) -> &str {
        &self.name
    }
    fn interactive(&self) -> bool {
        self.kind != KeyKind::Keyless
    }
    async fn login(
        &self,
        ui: &dyn AuthInteraction,
        opts: &LoginOptions,
    ) -> Result<ApiKeyCredential> {
        let secret = |s| ask(ui, opts, s, PromptKind::Secret);
        let text = |s| ask(ui, opts, s, PromptKind::Text);
        let mut key = None;
        let mut env = ProviderEnv::new();
        match self.kind {
            KeyKind::Keyless => {}
            KeyKind::CloudflareWorkers | KeyKind::CloudflareGateway => {
                key = Some(secret("Enter Cloudflare API key").await?);
                env.insert(
                    "CLOUDFLARE_ACCOUNT_ID".into(),
                    text("Enter Cloudflare account ID").await?,
                );
                if self.kind == KeyKind::CloudflareGateway {
                    env.insert(
                        "CLOUDFLARE_GATEWAY_ID".into(),
                        text("Enter Cloudflare AI Gateway ID").await?,
                    );
                }
            }
            KeyKind::Bedrock => {
                let method = choose(
                    ui,
                    opts,
                    "Select Amazon Bedrock authentication method:",
                    &[
                        ("bearer-token", "Bearer token"),
                        ("aws-profile", "AWS profile"),
                        ("credential-chain", "Existing AWS credential chain"),
                    ],
                )
                .await?;
                if method == "bearer-token" {
                    key = Some(secret("Enter Amazon Bedrock bearer token").await?);
                } else {
                    ui.notify(AuthEvent::Info{message:"Amazon Bedrock supports AWS profiles, IAM credentials, and role-based credentials.".into(),links:vec![("https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html".into(),Some("AWS credential provider chain".into()))]});
                    match method.as_str() {
                        "aws-profile" => {
                            env.insert("AWS_PROFILE".into(), text("Enter AWS profile name").await?);
                        }
                        "credential-chain" => {
                            text("Configure AWS credentials, then press Enter to continue").await?;
                        }
                        _ => {
                            return Err(Error::Auth(format!(
                                "Unknown Amazon Bedrock auth method: {method}"
                            )));
                        }
                    }
                }
            }
            KeyKind::Vertex => {
                let method = choose(
                    ui,
                    opts,
                    "Select Google Vertex AI authentication method:",
                    &[
                        ("api-key", "Google Cloud API key"),
                        ("adc", "Application Default Credentials"),
                        ("service-account", "Service account credentials file"),
                    ],
                )
                .await?;
                if method == "api-key" {
                    key = Some(secret("Enter Google Cloud API key").await?);
                } else {
                    let message = match method.as_str() {
                        "adc" => {
                            "Run `gcloud auth application-default login`, then provide the project and location."
                        }
                        "service-account" => {
                            "Provide a service account credentials file, project, and location."
                        }
                        _ => {
                            return Err(Error::Auth(format!(
                                "Unknown Google Vertex AI auth method: {method}"
                            )));
                        }
                    };
                    ui.notify(AuthEvent::Info {
                        message: message.into(),
                        links: vec![(
                            "https://cloud.google.com/docs/authentication/provide-credentials-adc"
                                .into(),
                            Some("Application Default Credentials".into()),
                        )],
                    });
                    env.insert(
                        "GOOGLE_CLOUD_PROJECT".into(),
                        text("Enter Google Cloud project ID").await?,
                    );
                    env.insert(
                        "GOOGLE_CLOUD_LOCATION".into(),
                        text("Enter Google Cloud location").await?,
                    );
                    if method == "service-account" {
                        env.insert(
                            "GOOGLE_APPLICATION_CREDENTIALS".into(),
                            text("Enter service account credentials file path").await?,
                        );
                    }
                }
            }
            _ => {
                let message = format!("Enter {}", self.name);
                key = Some(ask(ui, opts, &message, PromptKind::Secret).await?);
            }
        }
        Ok(ApiKeyCredential { key, env })
    }
    async fn resolve(
        &self,
        ctx: &dyn AuthContext,
        c: Option<&ApiKeyCredential>,
        signal: &CancellationToken,
    ) -> Result<Option<AuthResult>> {
        abortable(signal, async {
            let stored_key = c.and_then(|c| c.key.as_deref());
            let stored_env = || c.map(|c| c.env.clone()).unwrap_or_default();
            async fn value(
                ctx: &dyn AuthContext,
                c: Option<&ApiKeyCredential>,
                name: &str,
            ) -> Option<String> {
                if let Some(v) = c.and_then(|c| c.env.get(name)) {
                    Some(v.clone())
                } else {
                    ctx.env(name).await
                }
            }
            if self.kind == KeyKind::Keyless {
                return Ok(Some(AuthResult::default()));
            }
            if matches!(
                self.kind,
                KeyKind::CloudflareGateway | KeyKind::CloudflareWorkers
            ) {
                let key = if let Some(k) = stored_key {
                    Some(k.to_owned())
                } else {
                    ctx.env("CLOUDFLARE_API_KEY").await
                };
                let Some(key) = key.filter(|s| !s.is_empty()) else {
                    return Ok(None);
                };
                let Some(account) = value(ctx, c, "CLOUDFLARE_ACCOUNT_ID")
                    .await
                    .filter(|s| !s.is_empty())
                else {
                    return Ok(None);
                };
                let mut env = ProviderEnv::from([("CLOUDFLARE_ACCOUNT_ID".into(), account)]);
                let source = if c.is_some() {
                    "stored credential"
                } else {
                    "CLOUDFLARE_API_KEY"
                };
                if self.kind == KeyKind::CloudflareGateway {
                    let Some(gateway) = value(ctx, c, "CLOUDFLARE_GATEWAY_ID")
                        .await
                        .filter(|s| !s.is_empty())
                    else {
                        return Ok(None);
                    };
                    env.insert("CLOUDFLARE_GATEWAY_ID".into(), gateway);
                    let mut result = resolved(None, env, source);
                    result.auth.headers = Some(ProviderHeaders::from([
                        ("cf-aig-authorization".into(), Some(format!("Bearer {key}"))),
                        ("Authorization".into(), None),
                        ("x-api-key".into(), None),
                    ]));
                    return Ok(Some(result));
                }
                return Ok(Some(resolved(Some(key), env, source)));
            }
            if let Some(key) = stored_key.filter(|s| !s.is_empty()) {
                return Ok(Some(resolved(
                    Some(key.into()),
                    if self.kind == KeyKind::Vertex {
                        ProviderEnv::new()
                    } else {
                        stored_env()
                    },
                    "stored credential",
                )));
            }
            if self.kind == KeyKind::Anthropic {
                if let Some(token) = ctx
                    .env("ANTHROPIC_AUTH_TOKEN")
                    .await
                    .filter(|s| !s.is_empty())
                {
                    let mut r = resolved(None, ProviderEnv::new(), "ANTHROPIC_AUTH_TOKEN");
                    r.auth.headers = Some(ProviderHeaders::from([(
                        "Authorization".into(),
                        Some(format!("Bearer {token}")),
                    )]));
                    return Ok(Some(r));
                }
                for name in ["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"] {
                    if let Some(v) = ctx.env(name).await.filter(|s| !s.is_empty()) {
                        return Ok(Some(resolved(Some(v), ProviderEnv::new(), name)));
                    }
                }
                let mut env = ProviderEnv::new();
                for name in [
                    "ANTHROPIC_FEDERATION_RULE_ID",
                    "ANTHROPIC_ORGANIZATION_ID",
                    "ANTHROPIC_IDENTITY_TOKEN_FILE",
                ] {
                    let Some(v) = ctx.env(name).await.filter(|s| !s.is_empty()) else {
                        return Ok(None);
                    };
                    env.insert(name.into(), v);
                }
                for name in ["ANTHROPIC_SERVICE_ACCOUNT_ID", "ANTHROPIC_WORKSPACE_ID"] {
                    if let Some(v) = ctx.env(name).await.filter(|s| !s.is_empty()) {
                        env.insert(name.into(), v);
                    }
                }
                return Ok(Some(resolved(None, env, "workload identity federation")));
            }
            if self.kind == KeyKind::Vertex {
                if stored_key.is_none()
                    && let Some(key) = ctx
                        .env("GOOGLE_CLOUD_API_KEY")
                        .await
                        .filter(|s| !s.is_empty())
                {
                    return Ok(Some(resolved(
                        Some(key),
                        ProviderEnv::new(),
                        "GOOGLE_CLOUD_API_KEY",
                    )));
                }
                let path = value(ctx, c, "GOOGLE_APPLICATION_CREDENTIALS").await;
                let project = if let Some(p) = value(ctx, c, "GOOGLE_CLOUD_PROJECT").await {
                    Some(p)
                } else {
                    ctx.env("GCLOUD_PROJECT").await
                };
                let location = value(ctx, c, "GOOGLE_CLOUD_LOCATION").await;
                if project.is_some_and(|s| !s.is_empty())
                    && location.is_some_and(|s| !s.is_empty())
                    && ctx
                        .file_exists(
                            path.as_deref()
                                .unwrap_or("~/.config/gcloud/application_default_credentials.json"),
                        )
                        .await
                {
                    return Ok(Some(resolved(
                        None,
                        stored_env(),
                        if c.is_some() {
                            "stored credential"
                        } else {
                            "gcloud application default credentials"
                        },
                    )));
                }
                return Ok(None);
            }
            if self.kind == KeyKind::Bedrock {
                if let Some(key) = ctx
                    .env("AWS_BEARER_TOKEN_BEDROCK")
                    .await
                    .filter(|s| !s.is_empty())
                {
                    return Ok(Some(resolved(
                        None,
                        ProviderEnv::from([("AWS_BEARER_TOKEN_BEDROCK".into(), key)]),
                        "AWS_BEARER_TOKEN_BEDROCK",
                    )));
                }
                if let Some(profile) = value(ctx, c, "AWS_PROFILE").await.filter(|s| !s.is_empty())
                {
                    return Ok(Some(resolved(
                        None,
                        ProviderEnv::from([("AWS_PROFILE".into(), profile)]),
                        if c.is_some_and(|x| x.env.contains_key("AWS_PROFILE")) {
                            "stored credential"
                        } else {
                            "AWS_PROFILE"
                        },
                    )));
                }
                if let Some(id) = ctx.env("AWS_ACCESS_KEY_ID").await.filter(|s| !s.is_empty())
                    && let Some(secret) = ctx
                        .env("AWS_SECRET_ACCESS_KEY")
                        .await
                        .filter(|s| !s.is_empty())
                {
                    let mut env = ProviderEnv::from([
                        ("AWS_ACCESS_KEY_ID".into(), id),
                        ("AWS_SECRET_ACCESS_KEY".into(), secret),
                    ]);
                    if let Some(token) = ctx.env("AWS_SESSION_TOKEN").await {
                        env.insert("AWS_SESSION_TOKEN".into(), token);
                    }
                    return Ok(Some(resolved(None, env, "AWS access keys")));
                }
                for (name, source) in [
                    ("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "ECS task role"),
                    ("AWS_CONTAINER_CREDENTIALS_FULL_URI", "ECS task role"),
                    ("AWS_WEB_IDENTITY_TOKEN_FILE", "web identity token"),
                ] {
                    if let Some(v) = ctx.env(name).await.filter(|s| !s.is_empty()) {
                        return Ok(Some(resolved(
                            None,
                            ProviderEnv::from([(name.into(), v)]),
                            source,
                        )));
                    }
                }
                return Ok(None);
            }
            for env in &self.keys {
                if let Some(v) = ctx.env(env).await.filter(|s| !s.is_empty()) {
                    return Ok(Some(resolved(Some(v), ProviderEnv::new(), env)));
                }
            }
            Ok(None)
        })
        .await
    }
}
pub(crate) fn materialize<A>(model: &mut BaseModel<A>, env: &ProviderEnv) {
    if matches!(
        model.provider.as_str(),
        "cloudflare-workers-ai" | "cloudflare-ai-gateway"
    ) {
        for (key, placeholder) in [
            ("CLOUDFLARE_ACCOUNT_ID", "{CLOUDFLARE_ACCOUNT_ID}"),
            ("CLOUDFLARE_GATEWAY_ID", "{CLOUDFLARE_GATEWAY_ID}"),
        ] {
            if model.base_url.contains(placeholder)
                && let Some(value) = env.get(key)
            {
                model.base_url = model.base_url.replace(placeholder, value);
            }
        }
    }
}

pub fn radius_provider(
    id: impl Into<String>,
    name: impl Into<String>,
    gateway: Url,
) -> Result<Provider> {
    let id = id.into();
    let name = name.into();
    let mut provider = Provider::new(id.clone(), name.clone());
    provider.auth = ProviderAuth {
        api_key: Some(env_api_key_auth("Radius API key", ["RADIUS_API_KEY"])),
        oauth: Some(Arc::new(
            BuiltinOAuth::new(Flow::Radius)?.with_radius_gateway(name, gateway.clone())?,
        )),
    };
    provider.chat_apis.insert(Api::PiMessages);
    if gateway.as_str().trim_end_matches('/') == "https://radius.pi.dev" {
        provider.models = Catalog::embedded()?
            .chat_models("radius")
            .into_iter()
            .flat_map(|m| m.values())
            .cloned()
            .map(|mut m| {
                m.provider.clone_from(&id);
                AnyModel::Chat(m)
            })
            .collect::<Vec<_>>()
            .into();
    }
    provider.source = Some(Arc::new(RadiusSource { gateway }));
    Ok(provider)
}
struct RadiusSource {
    gateway: Url,
}
fn radius_models(value: &Value, provider: &str) -> Option<Vec<AnyModel>> {
    let base = value["baseUrl"].as_str()?;
    let models = value["models"]
        .as_array()?
        .iter()
        .filter_map(|raw| {
            let mut payload = raw.clone();
            let payload_obj = payload.as_object_mut()?;
            payload_obj.insert("api".into(), Value::String("pi-messages".into()));
            payload_obj.insert("provider".into(), Value::String(provider.into()));
            payload_obj.insert("baseUrl".into(), Value::String(base.into()));
            serde_json::from_value::<Model>(payload)
                .ok()
                .map(AnyModel::Chat)
        })
        .collect();
    Some(models)
}
#[async_trait]
impl ModelSource for RadiusSource {
    async fn refresh(
        &self,
        request: ModelSourceRequest<'_>,
    ) -> Result<Option<crate::models_store::ModelsStoreEntry>> {
        let mut restore = request.stored.cloned();
        if restore.is_none()
            && let Some(crate::auth::Credential::OAuth(credential)) = request.credential
            && let Some(value) = credential.details.get("gatewayConfig")
            && let Some(models) = radius_models(value, request.provider)
            && !models.is_empty()
        {
            restore = Some(crate::models_store::ModelsStoreEntry {
                models,
                checked_at: Some(unix_millis()),
                ..Default::default()
            });
        }
        if !request.allow_network {
            return Ok(restore);
        }
        let url = self.gateway.join("/v1/config")?;
        let mut req = request.client.get(url).header("accept", "application/json");
        if let Some(key) = request
            .credential
            .and_then(|c| match c {
                crate::auth::Credential::OAuth(o) => Some(o.access.as_str()),
                crate::auth::Credential::ApiKey(k) => k.key.as_deref(),
            })
            .filter(|s| !s.is_empty())
        {
            req = req.bearer_auth(key);
        }
        let value = abortable(request.signal, async {
            let resp = req.send().await?;
            if !resp.status().is_success() {
                let code = resp.status().as_u16();
                let body = resp.text().await?;
                return Err(Error::ModelSource(format!(
                    "Could not load Radius config from {}: {code}: {}",
                    self.gateway,
                    crate::http::truncate_error_text(body.trim(), 512)
                )));
            }
            Ok(resp.json::<Value>().await?)
        })
        .await?;
        let models = radius_models(&value, request.provider).ok_or_else(|| {
            Error::ModelSource(format!("Invalid Radius config from {}", self.gateway))
        })?;
        Ok(Some(crate::models_store::ModelsStoreEntry {
            models,
            checked_at: Some(unix_millis()),
            ..Default::default()
        }))
    }
}

pub fn openrouter_cost(pricing: &Value) -> ModelCost {
    fn rates(pricing: &Value, fallback: &ModelCostRates) -> ModelCostRates {
        let per_million = |key: &str, default: f64| {
            pricing[key]
                .as_str()
                .and_then(crate::http::parse_float_prefix)
                .filter(|n| *n >= 0.0)
                .map_or(default, |n| (n * 1e12).round() / 1e6)
        };
        ModelCostRates {
            input: per_million("prompt", fallback.input),
            output: per_million("completion", fallback.output),
            cache_read: per_million("input_cache_read", fallback.cache_read),
            cache_write: per_million("input_cache_write", fallback.cache_write),
        }
    }
    let base = rates(pricing, &ModelCostRates::default());
    let tiers = pricing["overrides"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tier| {
            if ["utc_start", "utc_end", "utc_days"]
                .into_iter()
                .any(|time| tier.get(time).is_some())
            {
                return None;
            }
            Some(ModelCostTier {
                input_tokens_above: tier["min_prompt_tokens"].as_f64()?,
                rates: rates(tier, &base),
            })
        })
        .collect();
    ModelCost { rates: base, tiers }
}

/// Use a key's OpenRouter model allowances without changing its image or classifier catalog.
pub async fn personalize_openrouter_catalog(
    catalog: Vec<AnyModel>,
    request: ModelSourceRequest<'_>,
) -> Result<Vec<AnyModel>> {
    let Some(key) = request
        .credential
        .and_then(|c| match c {
            crate::auth::Credential::OAuth(oauth) => Some(oauth.access.as_str()),
            crate::auth::Credential::ApiKey(api) => api.key.as_deref(),
        })
        .filter(|s| !s.is_empty())
    else {
        return Ok(catalog);
    };
    if !request.allow_network {
        return Ok(catalog);
    }
    let url = crate::http::endpoint(
        request.base_url.unwrap_or("https://openrouter.ai/api/v1"),
        "models/user",
    )?;
    let response = abortable(request.signal, async {
        let response = request
            .client
            .get(url)
            .header("accept", "application/json")
            .bearer_auth(key)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?;
        if matches!(response.status().as_u16(), 401 | 403 | 404) {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(Error::ModelSource(format!(
                "OpenRouter model list request failed: {}",
                response.status().as_u16()
            )));
        }
        Ok(Some(response.json::<Value>().await?))
    })
    .await?;
    let Some(response) = response else {
        return Ok(catalog);
    };
    let entries = response["data"]
        .as_array()
        .ok_or_else(|| Error::ModelSource("Invalid OpenRouter model list response".into()))?;
    let allowed: std::collections::HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|entry| Some((entry["id"].as_str()?, entry)))
        .collect();
    Ok(catalog
        .into_iter()
        .filter_map(|model| {
            let AnyModel::Chat(mut model) = model else {
                return Some(model);
            };
            let entry = allowed.get(model.id.as_str())?;
            let top = &entry["top_provider"];
            if let Some(window) = top["context_length"]
                .as_u64()
                .filter(|n| *n > 0)
                .or_else(|| entry["context_length"].as_u64().filter(|n| *n > 0))
            {
                model.context_window = window;
            }
            if let Some(max) = top["max_completion_tokens"].as_u64().filter(|n| *n > 0) {
                model.max_tokens = max;
            }
            if let Some(pricing) = entry.get("pricing").filter(|v| v.is_object()) {
                model.cost = openrouter_cost(pricing);
            }
            Some(AnyModel::Chat(model))
        })
        .collect())
}
