use std::{
    collections::HashSet,
    path::Path,
    sync::{Arc, Mutex, RwLock},
};

use indexmap::IndexMap;
use pi_rs_agent::{AgentError, CancellationToken, StreamFn, ThinkingLevel};
use pi_rs_ai::{
    AssistantContent, Context, Message, Model, Models, ModelsOptions, ProviderHeaders,
    ProviderRequestOptions, StreamOptions, StringOrBlocks, UserMessage, UserRole,
    auth::{AuthCheck, AuthInteraction, AuthType, Credential, CredentialStore, LoginOptions},
    registry::RefreshOptions,
};
use serde_json::{Value, json};

use super::{
    agent_session,
    event_bus::ExtensionEvent,
    extensions::{
        runner::ExtensionRunner,
        types::{VirtualModelRouteRequest, VirtualModelRouter},
    },
    model_registry::{ModelRegistry, ModelsJsonProvider},
    resolve_config_value::{
        get_config_value_env_var_names, is_command_config_value, is_config_value_configured,
    },
    settings_manager::{DEFAULT_HTTP_IDLE_TIMEOUT_MS, SettingsManager},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderAuthSource {
    Runtime,
    Stored,
    ModelsJsonCommand,
    Environment,
    Fallback,
    ModelsJsonKey,
    Virtual,
}

impl ProviderAuthSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Stored => "stored",
            Self::ModelsJsonCommand => "models_json_command",
            Self::Environment => "environment",
            Self::Fallback => "fallback",
            Self::ModelsJsonKey => "models_json_key",
            Self::Virtual => "virtual",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderAuthStatus {
    pub configured: bool,
    pub source: Option<ProviderAuthSource>,
    pub label: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ResolvedModelRoute {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub state: Option<Value>,
}

struct RegisteredVirtualModel {
    route: Option<VirtualModelRouter>,
}

#[derive(Default)]
struct RuntimeSnapshot {
    available: Vec<Model>,
    authorized: HashSet<String>,
    keys: IndexMap<String, String>,
    stored_providers: HashSet<String>,
    auth_checks: IndexMap<String, AuthCheck>,
    composition_errors: IndexMap<String, String>,
    availability_error: Option<String>,
    virtual_only_providers: HashSet<String>,
}

/// Catalog, resolved auth snapshot, and authenticated stream dispatch for one runtime.
pub struct ModelRuntime {
    registry: ModelRegistry,
    credentials: Arc<dyn CredentialStore>,
    available: Vec<Model>,
    snapshot: RwLock<RuntimeSnapshot>,
    scoped: Vec<super::model_resolver::ScopedModel>,
    virtual_models: RwLock<IndexMap<(String, String), RegisteredVirtualModel>>,
    virtual_states: Mutex<IndexMap<(String, String), Value>>,
    last_routed: Mutex<Option<(Model, Option<ThinkingLevel>)>>,
    http_proxy: Option<String>,
    http_idle_timeout_ms: u64,
    extensions: Option<Arc<ExtensionRunner>>,
}

impl ModelRuntime {
    pub async fn create(
        agent_dir: &Path,
        runner: &ExtensionRunner,
        diagnostics: &mut Vec<super::agent_session_runtime::RuntimeDiagnostic>,
    ) -> Result<Self, String> {
        let settings = SettingsManager::create_with_dirs(
            agent_dir,
            agent_dir,
            crate::core::settings_manager::CreateOptions::default(),
        );
        Self::create_with_settings(agent_dir, runner, &settings, diagnostics).await
    }

    pub async fn create_with_settings(
        agent_dir: &Path,
        runner: &ExtensionRunner,
        settings: &SettingsManager,
        diagnostics: &mut Vec<super::agent_session_runtime::RuntimeDiagnostic>,
    ) -> Result<Self, String> {
        let auth_path = agent_dir.join("auth.json");
        super::auth_storage::ensure_parent_dir(&auth_path)?;
        super::auth_storage::ensure_file_exists(&auth_path)?;
        super::auth_storage::ensure_file_exists(&agent_dir.join("models-store.json"))?;
        let credentials = crate::cli::auth_command::credential_store(agent_dir);

        let global_settings = settings.get_global_settings();
        let http_proxy = global_settings["httpProxy"]
            .as_str()
            .map(str::trim)
            .filter(|proxy| !proxy.is_empty())
            .map(str::to_owned);
        let idle_timeout_ms = settings.get_http_idle_timeout_ms()?;
        if (http_proxy.is_some() || idle_timeout_ms != DEFAULT_HTTP_IDLE_TIMEOUT_MS)
            && let Err(error) = super::http_dispatcher::client(settings)
        {
            diagnostics.push(super::agent_session_runtime::RuntimeDiagnostic::warning(
                error,
            ));
        }

        let models = Models::builtin(ModelsOptions {
            credentials: credentials.clone(),
            ..ModelsOptions::default()
        })
        .map_err(|error| error.to_string())?;
        if let Err(error) = crate::package_manager_cli::restore_model_catalogs(&models, agent_dir) {
            diagnostics.push(super::agent_session_runtime::RuntimeDiagnostic::warning(
                error,
            ));
        }
        let mut registry = ModelRegistry::new(models, Some(agent_dir.join("models.json")));
        let mut virtual_models = IndexMap::new();
        for extension in runner.extensions() {
            for vm in &extension.registration().virtual_models {
                virtual_models.insert(
                    (vm.provider.clone(), vm.id.clone()),
                    RegisteredVirtualModel {
                        route: vm.route.clone(),
                    },
                );
            }
        }
        let virtual_only = registry.apply_extensions(runner)?;
        let models = registry.models();
        let refresh_result = models
            .refresh(RefreshOptions {
                allow_network: false,
                ..RefreshOptions::default()
            })
            .await;
        let availability_error = (!refresh_result.errors.is_empty()).then(|| {
            refresh_result
                .errors
                .iter()
                .map(|(provider, error)| format!("{provider}: {error}"))
                .collect::<Vec<_>>()
                .join("; ")
        });
        let available = models
            .get_available(None, &CancellationToken::new())
            .await
            .map_err(|error| error.to_string())?;
        let mut authorized: HashSet<String> = available
            .iter()
            .map(|model| model.provider.clone())
            .collect();
        let keys: IndexMap<String, String> = registry
            .config()
            .providers
            .keys()
            .filter_map(|provider| {
                registry
                    .api_key_for_provider(provider)
                    .map(|key| (provider.clone(), key))
            })
            .collect();

        let stored_providers: HashSet<String> = credentials
            .list(&CancellationToken::new())
            .await
            .map(|list| list.into_iter().map(|info| info.provider_id).collect())
            .unwrap_or_default();

        let mut auth_checks = IndexMap::new();
        for provider_id in registry.provider_ids() {
            if let Ok(Some(check)) = models
                .check_auth(&provider_id, &CancellationToken::new())
                .await
            {
                auth_checks.insert(provider_id, check);
            }
        }

        let mut snapshot_available = available;
        let virtual_only_set: HashSet<String> = virtual_only.into_iter().collect();
        for provider in keys.keys().chain(virtual_only_set.iter()) {
            if authorized.insert(provider.clone()) {
                snapshot_available.extend(registry.list_for_provider(provider));
            }
        }
        for provider in &virtual_only_set {
            auth_checks.entry(provider.clone()).or_insert(AuthCheck {
                source: Some("virtual".to_owned()),
                auth_type: AuthType::ApiKey,
            });
        }

        let composition_errors = registry.composition_errors().clone();
        let runtime = Self {
            registry,
            credentials,
            available: snapshot_available.clone(),
            snapshot: RwLock::new(RuntimeSnapshot {
                available: snapshot_available,
                authorized,
                keys,
                stored_providers,
                auth_checks,
                composition_errors,
                availability_error,
                virtual_only_providers: virtual_only_set,
            }),
            scoped: Vec::new(),
            virtual_models: RwLock::new(virtual_models),
            virtual_states: Mutex::new(IndexMap::new()),
            last_routed: Mutex::new(None),
            http_proxy,
            http_idle_timeout_ms: idle_timeout_ms.max(0) as u64,
            extensions: None,
        };
        Ok(runtime)
    }

    pub fn with_extensions(mut self, runner: Arc<ExtensionRunner>) -> Self {
        self.extensions = Some(runner);
        self
    }

    pub fn registry(&self) -> &ModelRegistry {
        &self.registry
    }

    pub fn available(&self) -> &[Model] {
        &self.available
    }

    pub fn has_auth(&self, provider: &str) -> bool {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .authorized
            .contains(provider)
    }

    pub fn get_error(&self) -> Option<String> {
        let mut errors = Vec::new();
        if let Some(config_error) = self.registry.get_error() {
            errors.push(config_error.to_owned());
        }
        let snapshot = self.snapshot.read().unwrap_or_else(|e| e.into_inner());
        for (provider_id, error) in &snapshot.composition_errors {
            errors.push(format!("Provider \"{provider_id}\": {error}"));
        }
        if let Some(availability) = &snapshot.availability_error {
            errors.push(format!("Availability refresh: {availability}"));
        }
        (!errors.is_empty()).then(|| errors.join("\n\n"))
    }

    pub fn get_provider_auth_status(&self, provider: &str) -> ProviderAuthStatus {
        let snapshot = self.snapshot.read().unwrap_or_else(|e| e.into_inner());
        if snapshot.keys.contains_key(provider)
            && self
                .registry
                .config()
                .providers
                .get(provider)
                .and_then(|cfg| cfg.api_key.as_ref())
                .is_none()
        {
            return ProviderAuthStatus {
                configured: true,
                source: Some(ProviderAuthSource::Runtime),
                label: None,
            };
        }
        if snapshot.stored_providers.contains(provider) {
            return ProviderAuthStatus {
                configured: snapshot.authorized.contains(provider),
                source: Some(ProviderAuthSource::Stored),
                label: None,
            };
        }
        if let Some(configured) = configured_request_auth_status(
            self.registry.config().providers.get(provider),
            self.registry.extension_provider_config(provider),
        ) {
            return configured;
        }
        if snapshot.virtual_only_providers.contains(provider) {
            return ProviderAuthStatus {
                configured: true,
                source: Some(ProviderAuthSource::Virtual),
                label: Some("virtual".to_owned()),
            };
        }
        if let Some(check) = snapshot.auth_checks.get(provider) {
            return ProviderAuthStatus {
                configured: true,
                source: Some(ProviderAuthSource::Environment),
                label: check.source.clone(),
            };
        }
        ProviderAuthStatus {
            configured: false,
            source: None,
            label: None,
        }
    }

    pub fn is_using_oauth(&self, provider: &str) -> bool {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .auth_checks
            .get(provider)
            .is_some_and(|check| check.auth_type == AuthType::OAuth)
    }

    pub fn is_using_subscription(&self, provider: &str) -> bool {
        self.is_using_oauth(provider)
            && self
                .registry
                .get_provider(provider)
                .and_then(|p| p.auth.oauth)
                .is_some_and(|oauth| oauth.is_subscription())
    }

    pub fn set_runtime_api_key(&mut self, provider: &str, key: String) {
        let mut snapshot = self.snapshot.write().unwrap_or_else(|e| e.into_inner());
        snapshot.keys.insert(provider.to_owned(), key);
        if snapshot.authorized.insert(provider.to_owned()) {
            let extra = self.registry.list_for_provider(provider);
            snapshot.available.extend(extra.iter().cloned());
            self.available.extend(extra);
        }
    }

    pub fn remove_runtime_api_key(&mut self, provider: &str) {
        let mut snapshot = self.snapshot.write().unwrap_or_else(|e| e.into_inner());
        snapshot.keys.shift_remove(provider);
        let has_stored = snapshot.stored_providers.contains(provider);
        let has_env = snapshot.auth_checks.contains_key(provider);
        let is_virtual = snapshot.virtual_only_providers.contains(provider);
        if !has_stored && !has_env && !is_virtual {
            snapshot.authorized.remove(provider);
            snapshot
                .available
                .retain(|model| model.provider != provider);
            self.available.retain(|model| model.provider != provider);
        }
    }

    pub async fn synchronize_credentials(&self, provider: &str) -> Result<(), String> {
        let signal = CancellationToken::new();
        let refresh_result = self
            .registry
            .models()
            .refresh(RefreshOptions {
                allow_network: false,
                providers: Some(vec![provider.to_owned()]),
                signal: signal.clone(),
            })
            .await;
        if let Some(error) = refresh_result.errors.get(provider) {
            return Err(format!(
                "Credential synchronization failed for {provider}: {error}"
            ));
        }
        let available = self
            .registry
            .models()
            .get_available(None, &signal)
            .await
            .map_err(|error| error.to_string())?;
        let stored_providers: HashSet<String> = self
            .credentials
            .list(&signal)
            .await
            .map(|list| list.into_iter().map(|info| info.provider_id).collect())
            .unwrap_or_default();
        let check = self
            .registry
            .models()
            .check_auth(provider, &signal)
            .await
            .map_err(|error| error.to_string())?;

        let mut snapshot = self.snapshot.write().unwrap_or_else(|e| e.into_inner());
        snapshot.stored_providers = stored_providers;
        match check {
            Some(check) => {
                snapshot.auth_checks.insert(provider.to_owned(), check);
            }
            None => {
                snapshot.auth_checks.shift_remove(provider);
            }
        }
        let mut authorized: HashSet<String> =
            available.iter().map(|m| m.provider.clone()).collect();
        let mut combined_available = available;
        for extra_provider in snapshot
            .keys
            .keys()
            .chain(snapshot.virtual_only_providers.iter())
        {
            if authorized.insert(extra_provider.clone()) {
                combined_available.extend(self.registry.list_for_provider(extra_provider));
            }
        }
        snapshot.authorized = authorized;
        snapshot.available = combined_available;
        Ok(())
    }

    pub async fn login_provider(
        &self,
        provider: &str,
        kind: AuthType,
        ui: &dyn AuthInteraction,
        opts: &LoginOptions,
    ) -> Result<Credential, String> {
        let credential = self
            .registry
            .models()
            .login(provider, kind, ui, opts)
            .await
            .map_err(|error| error.to_string())?;
        self.synchronize_credentials(provider).await?;
        Ok(credential)
    }

    pub async fn logout_provider(
        &self,
        provider: &str,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        self.registry
            .models()
            .logout(provider, signal)
            .await
            .map_err(|error| error.to_string())?;
        self.synchronize_credentials(provider).await
    }

    pub fn set_scope(&mut self, scoped: Vec<super::model_resolver::ScopedModel>) {
        self.scoped = scoped;
    }

    pub fn get_physical_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.registry
            .get(provider, id)
            .filter(|model| model.api.as_str() != "pi-virtual")
    }

    pub fn last_routed_model(&self) -> Option<(Model, Option<ThinkingLevel>)> {
        self.last_routed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn register_virtual_model_router(
        &self,
        provider: &str,
        id: &str,
        router: VirtualModelRouter,
    ) {
        self.virtual_models
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (provider.to_owned(), id.to_owned()),
                RegisteredVirtualModel {
                    route: Some(router),
                },
            );
    }

    pub async fn resolve_model(
        &self,
        model: &Model,
        messages: &[Message],
        reason: &str,
        thinking_level: ThinkingLevel,
    ) -> Result<ResolvedModelRoute, String> {
        let name = format!("Virtual model {}/{}", model.provider, model.id);
        let router = {
            let guard = self
                .virtual_models
                .read()
                .unwrap_or_else(|e| e.into_inner());
            guard
                .get(&(model.provider.clone(), model.id.clone()))
                .and_then(|entry| entry.route.clone())
        }
        .ok_or_else(|| format!("{name} has no route handler registered."))?;

        let prev_state = self
            .virtual_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(model.provider.clone(), model.id.clone()))
            .cloned();

        let route = router(VirtualModelRouteRequest {
            reason: reason.to_owned(),
            model: model.clone(),
            thinking_level,
            messages: messages.to_vec(),
            state: prev_state,
        })
        .await?;

        let target = self
            .get_physical_model(&route.provider, &route.model_id)
            .ok_or_else(|| {
                format!(
                    "{name} routed to {}/{}, which is not a physical model.",
                    route.provider, route.model_id
                )
            })?;
        if !self.has_auth(&target.provider) {
            return Err(format!(
                "{name} routed to {}/{}, which has no credentials.",
                route.provider, route.model_id
            ));
        }

        let clamped_thinking = clamp_thinking_level(&target, route.thinking_level);
        if let Some(state) = &route.state {
            self.virtual_states
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((model.provider.clone(), model.id.clone()), state.clone());
        }
        *self.last_routed.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((target.clone(), Some(clamped_thinking)));

        Ok(ResolvedModelRoute {
            model: target,
            thinking_level: clamped_thinking,
            state: route.state,
        })
    }

    fn inject_proxy_env(&self, options: &mut ProviderRequestOptions) {
        if let Some(proxy) = &self.http_proxy {
            for key in ["HTTP_PROXY", "HTTPS_PROXY"] {
                let lower = key.to_ascii_lowercase();
                if !options.env.contains_key(key)
                    && !options.env.contains_key(&lower)
                    && std::env::var_os(key).is_none()
                    && std::env::var_os(&lower).is_none()
                {
                    options.env.insert(key.to_owned(), proxy.clone());
                }
            }
        }
        if options.timeout_ms.is_none() {
            options.timeout_ms = Some(if self.http_idle_timeout_ms == 0 {
                i32::MAX as u64
            } else {
                self.http_idle_timeout_ms
            });
        }
    }

    fn wire_extension_hooks(&self, options: &mut StreamOptions) {
        let Some(runner) = self.extensions.clone() else {
            return;
        };
        if runner.has_handlers(ExtensionEvent::BeforeProviderHeaders) {
            let runner_headers = runner.clone();
            let prev = options.request.transform_headers.take();
            options.request.transform_headers = Some(Arc::new(move |mut headers| {
                let runner = runner_headers.clone();
                let prev = prev.clone();
                Box::pin(async move {
                    if let Some(prev) = prev {
                        headers = prev(headers).await?;
                    }
                    let mut map = serde_json::Map::new();
                    for (k, v) in &headers {
                        map.insert(
                            k.clone(),
                            v.as_ref().map_or(Value::Null, |s| Value::String(s.clone())),
                        );
                    }
                    let patched = runner
                        .emit_before_provider_headers(Value::Object(map))
                        .await;
                    if let Some(obj) = patched.as_object() {
                        let mut next = ProviderHeaders::new();
                        for (k, v) in obj {
                            match v {
                                Value::Null => {
                                    next.insert(k.clone(), None);
                                }
                                Value::String(s) => {
                                    next.insert(k.clone(), Some(s.clone()));
                                }
                                other => {
                                    next.insert(k.clone(), Some(other.to_string()));
                                }
                            }
                        }
                        return Ok(next);
                    }
                    Ok(headers)
                })
            }));
        }
        if runner.has_handlers(ExtensionEvent::BeforeProviderRequest) {
            let runner_payload = runner.clone();
            let prev = options.request.on_payload.take();
            options.request.on_payload = Some(Arc::new(move |mut payload, model| {
                let runner = runner_payload.clone();
                let prev = prev.clone();
                Box::pin(async move {
                    if let Some(prev) = prev {
                        payload = prev(payload, model).await?;
                    }
                    Ok(runner.emit_before_provider_request(payload).await)
                })
            }));
        }
        if runner.has_handlers(ExtensionEvent::AfterProviderResponse) {
            let runner_resp = runner.clone();
            let prev = options.request.on_response.take();
            options.request.on_response = Some(Arc::new(move |response, model| {
                let runner = runner_resp.clone();
                let prev = prev.clone();
                Box::pin(async move {
                    if let Some(prev) = prev {
                        prev(response, model).await?;
                    }
                    runner
                        .emit(
                            ExtensionEvent::AfterProviderResponse,
                            json!({
                                "type": "after_provider_response",
                                "status": response.status,
                                "headers": response.headers,
                                "provider": model.provider,
                                "api": model.api,
                                "model": model.id,
                            }),
                        )
                        .await;
                    Ok(())
                })
            }));
        }
        if runner.has_handlers(ExtensionEvent::ProviderStreamEvent) {
            let runner_stream = runner;
            let prev = options.on_provider_stream_event.take();
            options.on_provider_stream_event = Some(Arc::new(move |data, model| {
                let runner = runner_stream.clone();
                let prev = prev.clone();
                Box::pin(async move {
                    if let Some(prev) = prev {
                        prev(data, model).await?;
                    }
                    runner
                        .emit(
                            ExtensionEvent::ProviderStreamEvent,
                            json!({
                                "type": "provider_stream_event",
                                "data": data,
                                "provider": model.provider,
                                "api": model.api,
                                "model": model.id,
                            }),
                        )
                        .await;
                    Ok(())
                })
            }));
        }
    }

    pub(crate) async fn summarize(
        &self,
        model: Model,
        prompt: String,
        signal: CancellationToken,
    ) -> Result<String, String> {
        let context = Context {
            system_prompt: Some(super::compaction::SUMMARIZATION_SYSTEM_PROMPT.to_owned()),
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: StringOrBlocks::String(prompt),
                timestamp: 0,
            })],
            tools: None,
        };
        let mut target_model = model.clone();
        let mut options = StreamOptions {
            max_tokens: Some(model.max_tokens.min(8192)),
            request: ProviderRequestOptions {
                api_key: self
                    .snapshot
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .keys
                    .get(&model.provider)
                    .cloned(),
                signal,
                ..ProviderRequestOptions::default()
            },
            ..StreamOptions::default()
        };
        if model.api.as_str() == "pi-virtual" {
            let route = self
                .resolve_model(&model, &context.messages, "direct", ThinkingLevel::Off)
                .await?;
            if route.model.provider != model.provider {
                options.request.api_key = self
                    .snapshot
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .keys
                    .get(&route.model.provider)
                    .cloned();
            }
            target_model = route.model;
            if target_model.max_tokens > 0 {
                options.max_tokens = Some(target_model.max_tokens.min(8192));
            }
            options.reasoning =
                (route.thinking_level != ThinkingLevel::Off).then_some(route.thinking_level);
        }
        self.inject_proxy_env(&mut options.request);
        let result = self
            .registry
            .models()
            .complete_simple(target_model, context, options)
            .await
            .map_err(|error| error.to_string())?;
        if result.stop_reason.failed() {
            return Err(result
                .error_message
                .clone()
                .unwrap_or_else(|| "Compaction failed".to_owned()));
        }
        let mut summary = String::new();
        for content in &result.content {
            if let AssistantContent::Text(text) = content {
                summary.push_str(&text.text);
            }
        }
        if summary.is_empty() {
            return Err("Model returned an empty summary".to_owned());
        }
        Ok(summary)
    }

    pub fn stream(self: &Arc<Self>) -> StreamFn {
        let runtime = self.clone();
        Arc::new(move |model, context, mut options| {
            let runtime = runtime.clone();
            Box::pin(async move {
                let messages = context.into_messages();
                let dispatch_model = if model.api.as_str() == "pi-virtual" {
                    let route = runtime
                        .resolve_model(
                            &model,
                            &messages,
                            "direct",
                            options.reasoning.unwrap_or(ThinkingLevel::Off),
                        )
                        .await
                        .map_err(AgentError::Callback)?;
                    if route.model.provider != model.provider {
                        options.request.api_key = None;
                        options.request.headers.clear();
                    }
                    if route.model.max_tokens > 0 {
                        options.max_tokens = Some(
                            options
                                .max_tokens
                                .map_or(route.model.max_tokens, |m| m.min(route.model.max_tokens)),
                        );
                    }
                    options.reasoning = (route.thinking_level != ThinkingLevel::Off)
                        .then_some(route.thinking_level);
                    route.model
                } else {
                    *runtime
                        .last_routed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = None;
                    model
                };
                if options.request.api_key.is_none() {
                    options.request.api_key = runtime
                        .snapshot
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .keys
                        .get(&dispatch_model.provider)
                        .cloned();
                }
                runtime.inject_proxy_env(&mut options.request);
                runtime.wire_extension_hooks(&mut options);
                Ok(runtime.registry.models().stream_simple(
                    dispatch_model,
                    Context {
                        messages,
                        ..Context::default()
                    },
                    options,
                ))
            })
        })
    }
}

fn clamp_thinking_level(model: &Model, requested: ThinkingLevel) -> ThinkingLevel {
    if !model.reasoning {
        return ThinkingLevel::Off;
    }
    let supported: Vec<ThinkingLevel> = model.supported_thinking_levels().collect();
    if supported.contains(&requested) {
        return requested;
    }
    let req_idx = ThinkingLevel::ALL
        .iter()
        .position(|l| *l == requested)
        .unwrap_or(0);
    for idx in (0..=req_idx).rev() {
        if supported.contains(&ThinkingLevel::ALL[idx]) {
            return ThinkingLevel::ALL[idx];
        }
    }
    supported.into_iter().next().unwrap_or(ThinkingLevel::Off)
}

fn configured_request_auth_status(
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ModelsJsonProvider>,
) -> Option<ProviderAuthStatus> {
    let from_ext = extension.and_then(|cfg| cfg.api_key.as_deref());
    let from_cfg = config.and_then(|cfg| cfg.api_key.as_deref());
    let value = from_cfg.or(from_ext)?;
    if is_command_config_value(value) {
        return Some(ProviderAuthStatus {
            configured: true,
            source: Some(ProviderAuthSource::ModelsJsonCommand),
            label: None,
        });
    }
    let names = get_config_value_env_var_names(value);
    if !names.is_empty() {
        return Some(if is_config_value_configured(value, None) {
            ProviderAuthStatus {
                configured: true,
                source: Some(ProviderAuthSource::Environment),
                label: Some(names.join(", ")),
            }
        } else {
            ProviderAuthStatus {
                configured: false,
                source: None,
                label: None,
            }
        });
    }
    Some(ProviderAuthStatus {
        configured: true,
        source: Some(if from_cfg.is_some() {
            ProviderAuthSource::ModelsJsonKey
        } else {
            ProviderAuthSource::Fallback
        }),
        label: None,
    })
}

impl agent_session::AuthStorage for ModelRuntime {
    fn has_configured_auth(&self, provider: &str) -> bool {
        self.has_auth(provider)
    }
}

impl agent_session::ModelRegistry for ModelRuntime {
    fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.registry.get(provider, id)
    }
    fn models(&self) -> Vec<Model> {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .available
            .clone()
    }
    fn has_configured_auth(&self, provider: &str) -> bool {
        self.has_auth(provider)
    }
    fn scoped_models(&self) -> Vec<(Model, Option<ThinkingLevel>)> {
        self.scoped
            .iter()
            .map(|scope| (scope.model.clone(), scope.thinking_level))
            .collect()
    }
    fn model_catalog(&self) -> Option<Models> {
        Some(self.registry.models().clone())
    }
    fn last_routed_model(&self) -> Option<(Model, Option<ThinkingLevel>)> {
        self.last_routed_model()
    }
    fn is_using_subscription(&self, provider: &str) -> bool {
        self.is_using_subscription(provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        event_bus::EventBus,
        extensions::{
            runner::ExtensionRunner,
            types::{
                Extension, ExtensionHost, ExtensionRegistration, SimpleExtension,
                VirtualModelRegistration, VirtualModelRouteResult,
            },
        },
    };
    use pi_rs_ai::faux::{FauxOptions, faux_assistant_message, faux_provider, faux_text};

    #[tokio::test]
    async fn virtual_model_routes_stream_through_registered_handler() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let bus = Arc::new(EventBus::new());
        let host = Arc::new(ExtensionHost::new(bus.clone()));
        let mut ext = SimpleExtension::new("virtual-test");
        let mut reg = ExtensionRegistration::new("virtual-test");
        reg.virtual_models.push(VirtualModelRegistration {
            provider: "virtual".into(),
            id: "smart-router".into(),
            definition: json!({ "name": "Smart Router", "thinkingLevels": ["off", "low"] }),
            extension_path: "virtual-test".into(),
            route: Some(Arc::new(|_req| {
                Box::pin(async move {
                    Ok(VirtualModelRouteResult {
                        provider: "faux".into(),
                        model_id: "faux-1".into(),
                        thinking_level: ThinkingLevel::Low,
                        state: Some(json!({ "picked": "faux-1" })),
                    })
                })
            })),
        });
        ext.set_registration(reg);
        let runner = ExtensionRunner::new(vec![Arc::new(ext)], host, bus, ".");
        let mut diagnostics = Vec::new();
        let mut runtime = ModelRuntime::create(dir.path(), &runner, &mut diagnostics).await?;
        let faux_handle = faux_provider(FauxOptions {
            provider: Some("faux".into()),
            ..FauxOptions::default()
        });
        faux_handle.set_responses([faux_assistant_message(vec![faux_text("routed reply")]).into()]);
        runtime
            .registry()
            .models()
            .set_provider(faux_handle.provider.clone());
        runtime.set_runtime_api_key("faux", "faux-key".into());
        let virtual_model = runtime
            .registry()
            .get("virtual", "smart-router")
            .ok_or("missing virtual model")?;
        assert_eq!(virtual_model.api.as_str(), "pi-virtual");
        assert_eq!(
            runtime.get_provider_auth_status("virtual").source,
            Some(ProviderAuthSource::Virtual)
        );

        let runtime = Arc::new(runtime);
        let stream_fn = runtime.stream();
        let stream = stream_fn(
            virtual_model,
            pi_rs_ai::TranscriptContext::default(),
            StreamOptions::default(),
        )
        .await
        .map_err(|e| e.to_string())?;
        let message = stream.result().await.map_err(|e| e.to_string())?;
        assert_eq!(message.provider.as_str(), "faux");
        assert_eq!(
            runtime.last_routed_model().map(|(m, _)| m.id.clone()),
            Some("faux-1".into())
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_auth_status_keys_error_and_proxy_settings_stay_in_sync() -> Result<(), String>
    {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let bus = Arc::new(EventBus::new());
        let host = Arc::new(ExtensionHost::new(bus.clone()));
        let runner = ExtensionRunner::new(Vec::new(), host, bus, ".");
        let settings = SettingsManager::in_memory(
            json!({
                "httpProxy": "http://127.0.0.1:8888",
                "httpIdleTimeoutMs": 45000
            }),
            crate::core::settings_manager::CreateOptions::default(),
        );
        let mut diagnostics = Vec::new();
        let mut runtime =
            ModelRuntime::create_with_settings(dir.path(), &runner, &settings, &mut diagnostics)
                .await?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(runtime.get_error().is_none());

        let mut req_opts = ProviderRequestOptions::default();
        runtime.inject_proxy_env(&mut req_opts);
        assert_eq!(req_opts.timeout_ms, Some(45_000));
        if std::env::var_os("HTTP_PROXY").is_none() && std::env::var_os("http_proxy").is_none() {
            assert_eq!(
                req_opts.env.get("HTTP_PROXY").map(String::as_str),
                Some("http://127.0.0.1:8888")
            );
        }

        runtime.set_runtime_api_key("anthropic", "sk-test-123".into());
        assert!(runtime.has_auth("anthropic"));
        let status = runtime.get_provider_auth_status("anthropic");
        assert!(status.configured);
        runtime.remove_runtime_api_key("anthropic");
        if std::env::var_os("ANTHROPIC_API_KEY").is_none() {
            assert!(!runtime.has_auth("anthropic"));
        }
        Ok(())
    }
}
