use std::{collections::HashSet, path::Path, sync::Arc};

use indexmap::IndexMap;
use pi_rs_agent::{CancellationToken, StreamFn, ThinkingLevel};
use pi_rs_ai::{
    AssistantContent, Context, Message, Model, Models, ModelsOptions, ProviderRequestOptions,
    StreamOptions, StringOrBlocks, UserMessage, UserRole, registry::RefreshOptions,
};

use super::{agent_session, model_registry::ModelRegistry, model_resolver::ScopedModel};

/// Catalog, resolved auth snapshot, and authenticated stream dispatch for one runtime.
pub struct ModelRuntime {
    registry: ModelRegistry,
    available: Vec<Model>,
    authorized: HashSet<String>,
    keys: IndexMap<String, String>,
    scoped: Vec<ScopedModel>,
}

impl ModelRuntime {
    pub async fn create(
        agent_dir: &Path,
        runner: &super::extensions::runner::ExtensionRunner,
        diagnostics: &mut Vec<super::agent_session_runtime::RuntimeDiagnostic>,
    ) -> Result<Self, String> {
        let auth_path = agent_dir.join("auth.json");
        super::auth_storage::ensure_parent_dir(&auth_path)?;
        super::auth_storage::ensure_file_exists(&auth_path)?;
        super::auth_storage::ensure_file_exists(&agent_dir.join("models-store.json"))?;
        let models = Models::builtin(ModelsOptions {
            credentials: crate::cli::auth_command::credential_store(agent_dir),
            ..ModelsOptions::default()
        })
        .map_err(|error| error.to_string())?;
        if let Err(error) = crate::package_manager_cli::restore_model_catalogs(&models, agent_dir) {
            diagnostics.push(super::agent_session_runtime::RuntimeDiagnostic::warning(
                error,
            ));
        }
        let mut registry = ModelRegistry::new(models, Some(agent_dir.join("models.json")));
        let virtual_only = registry.apply_extensions(runner)?;
        let models = registry.models();
        models
            .refresh(RefreshOptions {
                allow_network: false,
                ..RefreshOptions::default()
            })
            .await;
        let available = models
            .get_available(None, &CancellationToken::new())
            .await
            .map_err(|error| error.to_string())?;
        let authorized = available
            .iter()
            .map(|model| model.provider.clone())
            .collect();
        let keys = registry
            .config()
            .providers
            .keys()
            .filter_map(|provider| {
                registry
                    .api_key_for_provider(provider)
                    .map(|key| (provider.clone(), key))
            })
            .collect();
        let mut runtime = Self {
            registry,
            available,
            authorized,
            keys,
            scoped: Vec::new(),
        };
        for provider in runtime.keys.keys().chain(virtual_only.iter()) {
            if runtime.authorized.insert(provider.clone()) {
                runtime
                    .available
                    .extend(runtime.registry.list_for_provider(provider));
            }
        }
        Ok(runtime)
    }

    pub fn registry(&self) -> &ModelRegistry {
        &self.registry
    }

    pub fn available(&self) -> &[Model] {
        &self.available
    }

    pub fn has_auth(&self, provider: &str) -> bool {
        self.authorized.contains(provider)
    }

    pub fn set_runtime_api_key(&mut self, provider: &str, key: String) {
        self.keys.insert(provider.to_owned(), key);
        if self.authorized.insert(provider.to_owned()) {
            self.available
                .extend(self.registry.list_for_provider(provider));
        }
    }

    pub fn set_scope(&mut self, scoped: Vec<ScopedModel>) {
        self.scoped = scoped;
    }

    pub(crate) async fn summarize(
        &self,
        model: Model,
        prompt: String,
        signal: CancellationToken,
    ) -> Result<String, String> {
        let options = StreamOptions {
            max_tokens: Some(model.max_tokens.min(8192)),
            request: ProviderRequestOptions {
                api_key: self.keys.get(&model.provider).cloned(),
                signal,
                ..ProviderRequestOptions::default()
            },
            ..StreamOptions::default()
        };
        let context = Context {
            system_prompt: Some(super::compaction::SUMMARIZATION_SYSTEM_PROMPT.to_owned()),
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: StringOrBlocks::String(prompt),
                timestamp: 0,
            })],
            tools: None,
        };
        let result = self
            .registry
            .models()
            .complete_simple(model, context, options)
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
                if options.request.api_key.is_none() {
                    options.request.api_key = runtime.keys.get(&model.provider).cloned();
                }
                Ok(runtime.registry.models().stream_simple(
                    model,
                    Context {
                        messages: context.into_messages(),
                        ..Context::default()
                    },
                    options,
                ))
            })
        })
    }
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
        self.available.clone()
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
}
