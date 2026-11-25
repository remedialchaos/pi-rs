use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use futures_util::{
    StreamExt,
    future::{BoxFuture, join_all},
};
use indexmap::{IndexMap, IndexSet};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::auth::{
    AuthCheck, AuthContext, AuthInteraction, AuthOverrides, AuthResult, AuthType, Credential,
    CredentialStore, InMemoryCredentialStore, LoginOptions, ProviderAuth, SystemAuthContext,
    resolve_provider_auth,
};
use crate::catalog::Catalog;
use crate::classifier::{ClassifierOptions, classifier_failed, classifier_output};
use crate::error::{Error, Result};
use crate::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream, fail};
use crate::http::abortable;
use crate::model::{
    AnyModel, Api, BaseModel, ClassifierApi, ClassifierModel, ImageApi, ImageModel, Model,
    ModelInputModality, ModelType,
};
use crate::models_store::{InMemoryModelsStore, ModelsStore, ModelsStoreEntry};
use crate::openrouter_images::ImagesOptions;
use crate::providers::{builtin_providers, materialize};
use crate::request::{
    ClientIdentity, ProviderHeaders, ProviderRequestOptions, StreamOptions, merge_headers,
};
use crate::telemetry::RequestTelemetry;
use crate::transcript::normalize_context;
use crate::types::{
    AssistantImages, AssistantMessage, AssistantMessageEvent, ClassifierContext, ClassifierResult,
    Context, DeferredHandle, FinalMessage, ImagesContext, StopReason, TranscriptContext,
    unix_millis,
};

pub type ChatStream = Arc<
    dyn Fn(Model, TranscriptContext, StreamOptions, bool) -> AssistantMessageEventStream
        + Send
        + Sync,
>;
pub type DeferredStream =
    Arc<dyn Fn(Model, DeferredHandle, StreamOptions) -> AssistantMessageEventStream + Send + Sync>;
pub type DeferredCancel = Arc<
    dyn Fn(Model, DeferredHandle, StreamOptions) -> BoxFuture<'static, Result<()>> + Send + Sync,
>;
pub type ImageGenerator = Arc<
    dyn for<'a> Fn(
            &'a ImageModel,
            &'a ImagesContext,
            &'a ImagesOptions,
        ) -> BoxFuture<'a, AssistantImages>
        + Send
        + Sync,
>;
pub type Classifier = Arc<
    dyn for<'a> Fn(
            &'a ClassifierModel,
            &'a ClassifierContext,
            &'a ClassifierOptions,
        ) -> BoxFuture<'a, ClassifierResult>
        + Send
        + Sync,
>;

pub struct ModelSourceRequest<'a> {
    pub provider: &'a str,
    pub base_url: Option<&'a str>,
    pub credential: Option<&'a Credential>,
    pub stored: Option<&'a ModelsStoreEntry>,
    pub allow_network: bool,
    pub client: &'a reqwest::Client,
    pub signal: &'a CancellationToken,
}
#[async_trait]
pub trait ModelSource: Send + Sync {
    async fn refresh(&self, request: ModelSourceRequest<'_>) -> Result<Option<ModelsStoreEntry>>;
}
#[derive(Clone)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub base_url: Option<String>,
    pub headers: Option<IndexMap<String, String>>,
    pub auth: ProviderAuth,
    pub models: Arc<[AnyModel]>,
    pub source: Option<Arc<dyn ModelSource>>,
    pub chat_apis: IndexSet<Api>,
    pub image_apis: IndexSet<ImageApi>,
    pub classifier_apis: IndexSet<ClassifierApi>,
    pub chat: Option<ChatStream>,
    pub poll: Option<DeferredStream>,
    pub cancel: Option<DeferredCancel>,
    pub images: Option<ImageGenerator>,
    pub classify: Option<Classifier>,
}
impl Provider {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            base_url: None,
            headers: None,
            auth: ProviderAuth::default(),
            models: Arc::default(),
            source: None,
            chat_apis: IndexSet::new(),
            image_apis: IndexSet::new(),
            classifier_apis: IndexSet::new(),
            chat: None,
            poll: None,
            cancel: None,
            images: None,
            classify: None,
        }
    }
    pub fn with_models(mut self, models: Vec<AnyModel>) -> Self {
        self.models = models.into();
        self
    }
}
struct ProviderEntry {
    provider: Provider,
    published: RwLock<Option<Arc<[AnyModel]>>>,
    refresh: tokio::sync::Mutex<()>,
}
impl ProviderEntry {
    fn catalog(&self) -> Arc<[AnyModel]> {
        self.published
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .unwrap_or(&self.provider.models)
            .clone()
    }
    fn new(provider: Provider) -> Self {
        Self {
            provider,
            published: RwLock::new(None),
            refresh: tokio::sync::Mutex::new(()),
        }
    }
}
#[derive(Clone)]
pub struct ModelsOptions {
    pub credentials: Arc<dyn CredentialStore>,
    pub store: Arc<dyn ModelsStore>,
    pub auth_context: Arc<dyn AuthContext>,
    pub identity: ClientIdentity,
    pub client: Option<reqwest::Client>,
}
impl Default for ModelsOptions {
    fn default() -> Self {
        Self {
            credentials: Arc::new(InMemoryCredentialStore::default()),
            store: Arc::new(InMemoryModelsStore::default()),
            auth_context: Arc::new(SystemAuthContext),
            identity: ClientIdentity::default(),
            client: None,
        }
    }
}
#[derive(Clone)]
pub struct Models {
    providers: Arc<RwLock<IndexMap<String, Arc<ProviderEntry>>>>,
    opts: ModelsOptions,
}
impl Default for Models {
    fn default() -> Self {
        Self::new(ModelsOptions::default())
    }
}
#[derive(Clone)]
pub struct RefreshOptions {
    pub providers: Option<Vec<String>>,
    pub allow_network: bool,
    pub signal: CancellationToken,
}
impl Default for RefreshOptions {
    fn default() -> Self {
        Self {
            providers: None,
            allow_network: true,
            signal: CancellationToken::new(),
        }
    }
}
#[derive(Default)]
pub struct RefreshResult {
    pub refreshed: Vec<String>,
    pub errors: IndexMap<String, Error>,
}
impl Models {
    pub fn new(opts: ModelsOptions) -> Self {
        Self {
            providers: Arc::default(),
            opts,
        }
    }
    pub fn builtin(opts: ModelsOptions) -> Result<Self> {
        Self::from_catalog(Catalog::embedded()?, opts)
    }
    pub fn from_catalog(catalog: &Catalog, opts: ModelsOptions) -> Result<Self> {
        let models = Self::new(opts);
        for provider in builtin_providers(catalog)? {
            models.set_provider(provider);
        }
        Ok(models)
    }
    pub fn with_identity(&self, identity: ClientIdentity) -> Self {
        let mut models = self.clone();
        models.opts.identity = merge_identity(&models.opts.identity, identity);
        models
    }
    pub fn set_provider(&self, provider: Provider) {
        let id = provider.id.clone();
        self.providers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Arc::new(ProviderEntry::new(provider)));
    }
    pub fn delete_provider(&self, id: &str) -> bool {
        self.providers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .shift_remove(id)
            .is_some()
    }
    pub fn clear_providers(&self) {
        self.providers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    fn provider(&self, id: &str) -> Option<Arc<ProviderEntry>> {
        self.providers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }
    fn require(&self, id: &str) -> Result<Arc<ProviderEntry>> {
        self.provider(id).ok_or_else(|| Error::Provider(id.into()))
    }
    fn selected(&self, id: Option<&str>) -> Vec<Arc<ProviderEntry>> {
        if let Some(id) = id {
            return self.provider(id).into_iter().collect();
        }
        self.providers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }
    pub fn get_provider(&self, id: &str) -> Option<Provider> {
        self.provider(id).map(|e| e.provider.clone())
    }
    pub fn get_providers(&self) -> Vec<Provider> {
        self.selected(None)
            .into_iter()
            .map(|e| e.provider.clone())
            .collect()
    }
    pub fn get_all_models(&self, provider: Option<&str>) -> Vec<AnyModel> {
        self.selected(provider)
            .into_iter()
            .flat_map(|p| p.catalog().to_vec())
            .collect()
    }
    pub fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        let mut models = Vec::new();
        for p in self.selected(provider) {
            models.extend(p.catalog().iter().filter_map(|m| match m {
                AnyModel::Chat(m) => Some(m.clone()),
                _ => None,
            }));
        }
        models
    }
    pub fn get_models_of_type(&self, kind: ModelType, provider: Option<&str>) -> Vec<AnyModel> {
        let mut models = Vec::new();
        for p in self.selected(provider) {
            models.extend(
                p.catalog()
                    .iter()
                    .filter(|m| m.model_type() == kind)
                    .cloned(),
            );
        }
        models
    }
    pub fn get_model_of_type(&self, kind: ModelType, provider: &str, id: &str) -> Option<AnyModel> {
        self.provider(provider)?
            .catalog()
            .iter()
            .find(|m| m.model_type() == kind && m.id() == id)
            .cloned()
    }
    pub fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        match self.get_model_of_type(ModelType::Chat, provider, id)? {
            AnyModel::Chat(m) => Some(m),
            _ => None,
        }
    }
    pub async fn refresh(&self, options: RefreshOptions) -> RefreshResult {
        let entries = self.selected(None);
        let results = join_all(
            entries
                .into_iter()
                .filter(|p| {
                    p.provider.source.is_some()
                        && options
                            .providers
                            .as_ref()
                            .is_none_or(|ids| ids.contains(&p.provider.id))
                })
                .map(|provider| {
                    let opts = &options;
                    async move {
                        let result = self.refresh_one(&provider, opts).await;
                        (provider.provider.id.clone(), result)
                    }
                }),
        )
        .await;
        let mut out = RefreshResult::default();
        for (id, result) in results {
            match result {
                Ok(true) => out.refreshed.push(id),
                Ok(false) => {}
                Err(e) => {
                    out.errors.insert(id, e);
                }
            }
        }
        out
    }
    async fn refresh_one(&self, entry: &Arc<ProviderEntry>, opts: &RefreshOptions) -> Result<bool> {
        let provider = &entry.provider;
        let id = provider.id.as_str();
        let signal = &opts.signal;
        let Some(source) = &provider.source else {
            return Ok(false);
        };
        let _lock = abortable(signal, async { Ok(entry.refresh.lock().await) }).await?;
        let stored = self.opts.store.read(id, signal).await?;
        let credential = self.opts.credentials.read(id, signal).await?;
        let default;
        let client = if let Some(client) = &self.opts.client {
            client
        } else {
            default = reqwest::Client::builder().build()?;
            &default
        };
        let current = || self.provider(id).is_some_and(|p| Arc::ptr_eq(&p, entry));
        if !current() {
            return Ok(false);
        }
        let publish = |models: &[AnyModel]| {
            let models = models
                .iter()
                .filter(|m| m.provider() == id)
                .cloned()
                .collect::<Vec<_>>()
                .into();
            *entry.published.write().unwrap_or_else(|e| e.into_inner()) = Some(models);
        };
        if let Some(stored) = &stored {
            publish(&stored.models);
        }
        let fresh = abortable(
            signal,
            source.refresh(ModelSourceRequest {
                provider: id,
                base_url: provider.base_url.as_deref(),
                credential: credential.as_ref(),
                stored: stored.as_ref(),
                allow_network: opts.allow_network,
                client,
                signal,
            }),
        )
        .await?;
        if let Some(fresh) = fresh
            && current()
        {
            if stored.as_ref() != Some(&fresh) {
                self.opts.store.write(id, fresh.clone(), signal).await?;
            }
            if current() && !signal.is_cancelled() {
                publish(&fresh.models);
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub async fn check_auth(
        &self,
        id: &str,
        signal: &CancellationToken,
    ) -> Result<Option<AuthCheck>> {
        let Some(entry) = self.provider(id) else {
            return Ok(None);
        };
        abortable(signal, async {
            let saved = self.opts.credentials.read(id, signal).await?;
            if matches!(&saved, Some(Credential::OAuth(_))) {
                return Ok(entry.provider.auth.oauth.as_ref().map(|_| AuthCheck {
                    source: Some("OAuth".into()),
                    auth_type: AuthType::OAuth,
                }));
            }
            let saved = match &saved {
                Some(Credential::ApiKey(key)) => Some(key),
                _ => None,
            };
            if let Some(auth) = &entry.provider.auth.api_key {
                auth.check(self.opts.auth_context.as_ref(), saved, signal)
                    .await
            } else {
                Ok(None)
            }
        })
        .await
    }
    pub async fn get_all_available(
        &self,
        id: Option<&str>,
        signal: &CancellationToken,
    ) -> Result<Vec<AnyModel>> {
        let mut models = Vec::new();
        for entry in self.selected(id) {
            let p = &entry.provider;
            if self.check_auth(&p.id, signal).await?.is_none() {
                continue;
            }
            let saved = if p.id == "github-copilot" {
                self.opts.credentials.read(&p.id, signal).await?
            } else {
                None
            };
            let mut available = None;
            if let Some(Credential::OAuth(saved)) = &saved
                && let Some(ids) = saved
                    .details
                    .get("availableModelIds")
                    .and_then(serde_json::Value::as_array)
                && ids.iter().all(serde_json::Value::is_string)
            {
                available = Some(
                    ids.iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<IndexSet<_>>(),
                );
            }
            models.extend(
                entry
                    .catalog()
                    .iter()
                    .filter(|m| {
                        m.model_type() != ModelType::Chat
                            || available.as_ref().is_none_or(|a| a.contains(m.id()))
                    })
                    .cloned(),
            );
        }
        Ok(models)
    }
    pub async fn get_available(
        &self,
        id: Option<&str>,
        signal: &CancellationToken,
    ) -> Result<Vec<Model>> {
        Ok(self
            .get_all_available(id, signal)
            .await?
            .into_iter()
            .filter_map(|m| match m {
                AnyModel::Chat(m) => Some(m),
                _ => None,
            })
            .collect())
    }
    pub async fn get_auth(&self, id: &str, opts: &AuthOverrides) -> Result<Option<AuthResult>> {
        let Some(entry) = self.provider(id) else {
            return Ok(None);
        };
        resolve_provider_auth(
            id,
            &entry.provider.auth,
            self.opts.credentials.clone(),
            self.opts.auth_context.as_ref(),
            opts,
        )
        .await
    }
    pub async fn get_model_auth<A>(
        &self,
        model: &BaseModel<A>,
        opts: &AuthOverrides,
    ) -> Result<Option<AuthResult>> {
        let result = self.get_auth(&model.provider, opts).await?;
        Ok(result.map(|mut r| {
            if let Some(h) = &model.headers {
                r.auth.headers = Some(merge_headers(
                    r.auth.headers.take().unwrap_or_default(),
                    h.iter()
                        .map(|(k, v)| (k.clone(), Some(v.clone())))
                        .collect(),
                ));
            }
            r
        }))
    }
    pub async fn login(
        &self,
        id: &str,
        kind: AuthType,
        ui: &dyn AuthInteraction,
        opts: &LoginOptions,
    ) -> Result<Credential> {
        let entry = self.require(id)?;
        let provider = &entry.provider;
        let signal = &opts.signal;
        let credential = match kind {
            AuthType::ApiKey => {
                if let Some(key) = &provider.auth.api_key
                    && key.interactive()
                {
                    Some(Credential::ApiKey(
                        abortable(signal, key.login(ui, opts)).await?,
                    ))
                } else {
                    None
                }
            }
            AuthType::OAuth => {
                if let Some(oauth) = &provider.auth.oauth {
                    Some(Credential::OAuth(
                        abortable(signal, oauth.login(ui, opts)).await?,
                    ))
                } else {
                    None
                }
            }
        }
        .ok_or_else(|| {
            Error::Auth(format!(
                "{} does not support {} login",
                provider.name,
                match kind {
                    AuthType::ApiKey => "api_key",
                    AuthType::OAuth => "oauth",
                }
            ))
        })?;
        let next = credential.clone();
        self.opts
            .credentials
            .modify(
                id,
                Box::new(move |_| Box::pin(async { Ok(Some(next)) })),
                signal,
            )
            .await?;
        Ok(credential)
    }
    pub async fn logout(&self, id: &str, signal: &CancellationToken) -> Result<()> {
        self.opts.credentials.delete(id, signal).await
    }
    async fn apply<M, A>(
        &self,
        provider: &Provider,
        model: &mut BaseModel<A>,
        options: &mut ProviderRequestOptions<M>,
        session: Option<&str>,
    ) -> Result<()> {
        let overrides = AuthOverrides {
            api_key: options.api_key.clone(),
            env: options.env.clone(),
            signal: options.signal.clone(),
            ..AuthOverrides::default()
        };
        if let Some(config) = &provider.base_url
            && model.base_url.is_empty()
        {
            model.base_url.clone_from(config);
        }
        if let Some(headers) = &provider.headers {
            let merged = merge_headers(
                headers
                    .iter()
                    .map(|(k, v)| (k.clone(), Some(v.clone())))
                    .collect(),
                model
                    .headers
                    .take()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(k, v)| (k, Some(v)))
                    .collect(),
            );
            model.headers = Some(
                merged
                    .into_iter()
                    .filter_map(|(k, v)| v.map(|v| (k, v)))
                    .collect(),
            );
        }
        let AuthResult { auth, mut env, .. } = self
            .get_model_auth(model, &overrides)
            .await?
            .ok_or_else(|| Error::Auth(format!("Provider is not configured: {}", provider.id)))?;
        if options.api_key.is_none() {
            options.api_key = auth.api_key;
        }
        if let Some(base) = auth.base_url {
            model.base_url = base;
        }
        env.extend(std::mem::take(&mut options.env));
        options.env = env;
        materialize(model, &options.env);
        let mut headers = ProviderHeaders::new();
        let opencode = matches!(model.provider.as_str(), "opencode" | "opencode-go")
            || Url::parse(&model.base_url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .as_deref()
                == Some("opencode.ai");
        if opencode && let Some(session) = session.filter(|s| !s.is_empty()) {
            headers.insert("x-opencode-session".into(), Some(session.into()));
        }
        headers = merge_headers(
            merge_headers(headers, auth.headers.unwrap_or_default()),
            std::mem::take(&mut options.headers),
        );
        if let Some(transform) = options.transform_headers.take() {
            headers = abortable(&options.signal, transform(headers)).await?;
        }
        options.headers = headers;
        options.identity =
            merge_identity(&self.opts.identity, std::mem::take(&mut options.identity));
        if options.client.is_none() {
            options.client.clone_from(&self.opts.client);
        }
        Ok(())
    }
    fn stream_request(
        &self,
        model: Model,
        context: StreamRequest,
        opts: StreamOptions,
    ) -> AssistantMessageEventStream {
        let (send, receiver) = AssistantMessageEventStream::channel();
        let this = self.clone();
        tokio::spawn(async move {
            let poll = matches!(&context, StreamRequest::Deferred(_));
            let reason = opts.reasoning;
            let telemetry = opts.telemetry.clone();
            let terminal = if let Some(parent) = telemetry {
                RequestTelemetry::observe(parent, model, reason, poll, |observer, model| {
                    this.forward(&send, model, context, opts, Some(observer))
                })
                .await
            } else {
                this.forward(&send, model, context, opts, None).await
            };
            send.send(terminal);
        });
        receiver
    }
    pub fn stream(
        &self,
        model: Model,
        context: Context,
        opts: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.stream_request(
            model,
            StreamRequest::Chat(normalize_context(context), false),
            opts,
        )
    }
    pub fn stream_simple(
        &self,
        model: Model,
        context: Context,
        opts: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.stream_request(
            model,
            StreamRequest::Chat(normalize_context(context), true),
            opts,
        )
    }
    pub async fn complete(
        &self,
        model: Model,
        context: Context,
        opts: StreamOptions,
    ) -> Result<FinalMessage> {
        self.stream(model, context, opts).result().await
    }
    pub async fn complete_simple(
        &self,
        model: Model,
        context: Context,
        opts: StreamOptions,
    ) -> Result<FinalMessage> {
        self.stream_simple(model, context, opts).result().await
    }
    pub fn stream_deferred(
        &self,
        model: Model,
        handle: DeferredHandle,
        opts: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.stream_request(model, StreamRequest::Deferred(handle), opts)
    }
    pub async fn fetch_deferred(
        &self,
        model: Model,
        handle: DeferredHandle,
        opts: StreamOptions,
    ) -> Result<FinalMessage> {
        self.stream_deferred(model, handle, opts).result().await
    }
    pub async fn cancel_deferred(
        &self,
        mut model: Model,
        handle: DeferredHandle,
        mut opts: StreamOptions,
    ) -> Result<()> {
        let entry = self.require(&model.provider)?;
        let provider = &entry.provider;
        let cancel = provider
            .cancel
            .as_ref()
            .ok_or_else(|| unsupported(provider, "deferred responses"))?;
        self.apply(
            provider,
            &mut model.base,
            &mut opts.request,
            opts.session_id.as_deref(),
        )
        .await?;
        cancel(model, handle, opts).await
    }
    async fn forward(
        &self,
        send: &AssistantMessageEventSender,
        mut actual: Model,
        request: StreamRequest,
        mut opts: StreamOptions,
        mut telemetry: Option<RequestTelemetry>,
    ) -> AssistantMessageEvent {
        let poll = matches!(&request, StreamRequest::Deferred(_));
        let pending = AssistantMessage::pending(&actual);
        let runtime = async {
            let entry = self.require(&actual.provider)?;
            let provider = &entry.provider;
            self.apply(
                provider,
                &mut actual.base,
                &mut opts.request,
                opts.session_id.as_deref(),
            )
            .await?;
            match request {
                StreamRequest::Chat(context, simple) => {
                    if let Some(stream) = &provider.chat {
                        return Ok(stream(actual, context, opts, simple));
                    }
                    if !provider.chat_apis.contains(&actual.api) {
                        return Err(Error::Provider(format!(
                            "No API provider registered for api: {}",
                            actual.api
                        )));
                    }
                    azure_chat(&mut actual, &mut opts)?;
                    dispatch(actual, context, opts, simple)
                }
                StreamRequest::Deferred(handle) => {
                    let fetch = provider
                        .poll
                        .as_ref()
                        .ok_or_else(|| unsupported(provider, "deferred responses"))?;
                    Ok(fetch(actual, handle, opts))
                }
            }
        }
        .await;
        let terminal = match runtime {
            Ok(mut stream) => loop {
                match stream.next().await {
                    Some(event) => {
                        if let Some(observer) = &mut telemetry {
                            observer.event(&event);
                        }
                        match event {
                            AssistantMessageEvent::Done { message, .. }
                            | AssistantMessageEvent::Error { error: message, .. } => break message,
                            other => send.send(other),
                        }
                    }
                    None => break Arc::new(fail(pending, Error::MissingTerminalEvent)),
                }
            },
            Err(error) => Arc::new(fail(pending, error)),
        };
        let event = if terminal.stop_reason.failed() {
            AssistantMessageEvent::Error {
                reason: terminal.stop_reason,
                error: terminal.clone(),
            }
        } else {
            AssistantMessageEvent::Done {
                reason: terminal.stop_reason,
                message: terminal.clone(),
            }
        };
        if let Some(mut observer) = telemetry {
            observer.event(&event);
            observer.result(&terminal, poll);
        }
        event
    }
    pub async fn generate_images(
        &self,
        mut model: ImageModel,
        context: &ImagesContext,
        mut opts: ImagesOptions,
    ) -> AssistantImages {
        let failed = |model: &ImageModel, e: Error, signal: &CancellationToken| AssistantImages {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            output: vec![],
            response_id: None,
            usage: None,
            stop_reason: if signal.is_cancelled() || matches!(e, Error::Aborted) {
                StopReason::Aborted
            } else {
                StopReason::Error
            },
            error_message: Some(e.to_string()),
            timestamp: unix_millis(),
        };
        let result = async {
            let entry = self.require(&model.provider)?;
            let p = &entry.provider;
            if p.images.is_none() && !p.image_apis.contains(&model.api) {
                return Err(unsupported(p, "image generation"));
            }
            self.apply(p, &mut model.base, &mut opts, None).await?;
            if let Some(generate) = &p.images {
                return Ok(generate(&model, context, &opts).await);
            }
            match model.api {
                ImageApi::OpenRouterImages => {
                    Ok(crate::openrouter_images::generate_images(&model, context, &opts).await)
                }
                _ => Err(unsupported(p, "image generation")),
            }
        }
        .await;
        result.unwrap_or_else(|e| failed(&model, e, &opts.signal))
    }
    pub async fn classify(
        &self,
        mut model: ClassifierModel,
        context: &ClassifierContext,
        mut opts: ClassifierOptions,
    ) -> ClassifierResult {
        let result = async {
            let entry = self.require(&model.provider)?;
            let p = &entry.provider;
            if !context.images.is_empty() && !model.input.contains(&ModelInputModality::Image) {
                return Err(Error::Protocol(format!(
                    "Classifier model {} does not support image input",
                    model.id
                )));
            }
            if p.classify.is_none() && !p.classifier_apis.contains(&model.api) {
                return Err(unsupported(p, "classification"));
            }
            self.apply(p, &mut model.base, &mut opts.request, None)
                .await?;
            if model.provider == "github-copilot"
                && model.api == ClassifierApi::TypeSafeSystemOne
                && !model.base_url.trim_end_matches('/').ends_with("/v1")
            {
                let end = model.base.base_url.trim_end_matches('/').len();
                model.base.base_url.truncate(end);
                model.base_url.push_str("/v1");
            }
            if let Some(classify) = &p.classify {
                return Ok(classify(&model, context, &opts).await);
            }
            match model.api {
                ClassifierApi::TypeSafeSystemOne | ClassifierApi::CloudflareWorkersAiSystemOne => {
                    Ok(crate::classifier::classify_system_one(&model, context, &opts).await)
                }
                ClassifierApi::LlamaCppClassify => {
                    Ok(crate::llama_cpp::classify(&model, context, &opts).await)
                }
                ClassifierApi::OpenAiDecisions => {
                    Ok(crate::openai_decisions::classify(&model, context, &opts).await)
                }
                _ => Err(unsupported(p, "classification")),
            }
        }
        .await;
        result.unwrap_or_else(|error| {
            let mut output = classifier_output(&model);
            classifier_failed(&mut output, error, &opts.request);
            output
        })
    }
}
enum StreamRequest {
    Chat(TranscriptContext, bool),
    Deferred(DeferredHandle),
}
fn unsupported(provider: &Provider, operation: &str) -> Error {
    Error::Protocol(format!(
        "Provider {} does not support {operation}",
        provider.id
    ))
}
pub fn merge_identity(base: &ClientIdentity, override_: ClientIdentity) -> ClientIdentity {
    ClientIdentity {
        user_agent: override_.user_agent.or_else(|| base.user_agent.clone()),
        version: override_.version.or_else(|| base.version.clone()),
        attribution: override_.attribution.or_else(|| base.attribution.clone()),
    }
}
pub(crate) fn azure_chat(model: &mut Model, options: &mut StreamOptions) -> Result<()> {
    if model.api != Api::OpenAiCompletions || model.provider != "azure" {
        return Ok(());
    }
    let mut base = crate::responses::resolve_azure_url(model, options)?;
    base.set_query(None);
    base.path_segments_mut()
        .map_err(|()| Error::Protocol("Invalid Azure OpenAI base URL".into()))?
        .pop();
    model.base_url = base.into();
    let id = crate::responses::azure_deployment(model, options);
    if id != model.id {
        let id = id.into_owned();
        let override_ = options.request.on_payload.take();
        options.request.on_payload = Some(Arc::new(move |mut value, model| {
            value["model"] = serde_json::Value::String(id.clone());
            let call = override_.clone();
            Box::pin(async move {
                if let Some(call) = call {
                    call(value, model).await
                } else {
                    Ok(value)
                }
            })
        }));
    }
    Ok(())
}
pub(crate) fn dispatch(
    model: Model,
    context: TranscriptContext,
    options: StreamOptions,
    simple: bool,
) -> Result<AssistantMessageEventStream> {
    if simple {
        match model.api {
            Api::AnthropicMessages => {
                return crate::anthropic::stream_simple(model, context, options);
            }
            Api::OpenAiCompletions => {
                return crate::openai_completions::stream_simple(model, context, options);
            }
            Api::OpenAiResponses | Api::AzureOpenAiResponses => {
                return crate::responses::stream_simple(model, context, options);
            }
            Api::OpenAiCodexResponses => {
                return crate::codex::stream_simple(model, context, options);
            }
            Api::BedrockConverseStream => {
                return Ok(crate::bedrock::stream_simple(model, context, options));
            }
            Api::GoogleGenerativeAi | Api::GoogleVertex => {
                return crate::google::stream_simple(model, context, options);
            }
            Api::MistralConversations => {
                return crate::mistral::stream_simple(model, context, options);
            }
            _ => {}
        }
    }
    Ok(match model.api {
        Api::AnthropicMessages => crate::anthropic::stream(model, context, options),
        Api::OpenAiCompletions => crate::openai_completions::stream(model, context, options),
        Api::OpenAiResponses | Api::AzureOpenAiResponses => {
            crate::responses::stream(model, context, options)
        }
        Api::OpenAiCodexResponses => crate::codex::stream(model, context, options),
        Api::BedrockConverseStream => crate::bedrock::stream(model, context, options),
        Api::GoogleGenerativeAi | Api::GoogleVertex => {
            crate::google::stream(model, context, options)
        }
        Api::MistralConversations => crate::mistral::stream(model, context, options),
        Api::PiMessages => crate::pi_messages::stream(model, context, options),
        Api::Custom(ref api) => {
            return Err(Error::Protocol(format!(
                "No API provider registered for api: {api}"
            )));
        }
    })
}
