//! Small SDK surface for embedding a coding-agent session.
//!
//! Upstream's `core/sdk.ts` creates an `AgentSession` with resolved model, settings, tools, and
//! resource loader, and re-exports that session to callers. This port keeps the same shape:
//! [`PiSdkOptions`] gathers defaults and injected services, [`create_sdk_session`] constructs the
//! session, and [`PiSdk`] exposes `prompt`, `subscribe`, and `close`. No network or filesystem
//! access happens here; all services can be injected for offline use.

use std::sync::Arc;

use pi_rs_agent::{Agent, AgentInitialState, AgentOptions, StreamFn, ThinkingLevel};
use pi_rs_ai::Model;

use super::agent_session::{
    AgentSession, AgentSessionConfig, AuthStorage, Compactor, ExtensionRunner,
    InMemorySessionManager, InMemorySettings, ModelRegistry, NoopExtensionRunner,
    PromptDisposition, PromptOptions, Result, SessionError, SessionEventListener, SessionManager,
    SessionStats, SettingsStore, StaticAuth, TailCompactor,
};

/// Options for [`create_sdk_session`].
pub struct PiSdkOptions {
    pub cwd: String,
    pub agent_dir: Option<String>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub session_manager: Option<Arc<dyn SessionManager>>,
    pub settings: Option<Arc<dyn SettingsStore>>,
    pub auth: Option<Arc<dyn AuthStorage>>,
    pub models: Option<Arc<dyn ModelRegistry>>,
    pub compactor: Option<Arc<dyn Compactor>>,
    pub extensions: Option<Arc<dyn ExtensionRunner>>,
    pub stream_fn: Option<StreamFn>,
    /// Message used after a retryable provider failure.
    pub retry_prompt: Option<String>,
}

impl Default for PiSdkOptions {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().map_or_else(
                |_| ".".to_string(),
                |path| path.to_string_lossy().into_owned(),
            ),
            agent_dir: None,
            model: None,
            thinking_level: None,
            session_manager: None,
            settings: None,
            auth: None,
            models: None,
            compactor: None,
            extensions: None,
            stream_fn: None,
            retry_prompt: None,
        }
    }
}

/// Handle to a constructed SDK session.
#[derive(Clone)]
pub struct PiSdk {
    session: AgentSession,
}

impl PiSdk {
    pub fn session(&self) -> &AgentSession {
        &self.session
    }

    pub async fn prompt(
        &self,
        text: impl Into<String>,
        options: PromptOptions,
    ) -> Result<PromptDisposition> {
        self.session.prompt(text, options).await
    }

    pub fn subscribe(&self, listener: SessionEventListener) -> usize {
        self.session.subscribe(listener)
    }

    pub fn unsubscribe(&self, id: usize) -> bool {
        self.session.unsubscribe(id)
    }

    pub fn stats(&self) -> SessionStats {
        self.session.stats()
    }

    /// Dispose listeners and abort any active run.
    pub fn close(&self) {
        self.session.dispose();
    }
}

/// Create an SDK session from options, resolving defaults only when an option is absent.
pub fn create_sdk_session(options: PiSdkOptions) -> Result<PiSdk> {
    let stream_fn = options.stream_fn.ok_or_else(|| {
        SessionError::Extension("A stream function is required; pass options.stream_fn".to_string())
    })?;
    let model = options.model;
    let provider = model
        .as_ref()
        .map_or_else(String::new, |model| model.provider.clone());
    let auth: Arc<dyn AuthStorage> = options.auth.unwrap_or_else(|| {
        if provider.is_empty() {
            Arc::new(StaticAuth::default())
        } else {
            Arc::new(StaticAuth::with_providers([provider.clone()]))
        }
    });
    let models: Arc<dyn ModelRegistry> = options.models.unwrap_or_else(|| {
        Arc::new(crate::core::agent_session::InMemoryModelRegistry::new(
            model.clone().into_iter().collect(),
            auth.clone(),
        ))
    });
    let settings: Arc<dyn SettingsStore> = options
        .settings
        .unwrap_or_else(|| Arc::new(InMemorySettings::default()));
    let session_manager: Arc<dyn SessionManager> = options
        .session_manager
        .unwrap_or_else(|| Arc::new(InMemorySessionManager::new(options.cwd.clone())));
    let compactor: Arc<dyn Compactor> = options
        .compactor
        .unwrap_or_else(|| Arc::new(TailCompactor { keep_recent: 3 }));
    let extensions: Arc<dyn ExtensionRunner> = options
        .extensions
        .unwrap_or_else(|| Arc::new(NoopExtensionRunner));
    let agent = Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            model,
            thinking_level: options.thinking_level,
            ..AgentInitialState::default()
        },
        stream_fn: Some(stream_fn),
        ..AgentOptions::default()
    })?;
    let session = AgentSession::new(AgentSessionConfig {
        agent,
        session_manager,
        settings,
        auth,
        models,
        compactor,
        extensions,
        cwd: options.cwd,
        retry_prompt: options.retry_prompt,
    })?;
    Ok(PiSdk { session })
}

/// Upstream-compatible alias for [`create_sdk_session`].
pub fn create_agent_session(options: PiSdkOptions) -> Result<PiSdk> {
    create_sdk_session(options)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::core::agent_session::AgentSessionEvent;
    use pi_rs_ai::{
        Models, ModelsOptions, StopReason,
        faux::{FauxOptions, FauxStep, faux_assistant_message, faux_provider, faux_text},
    };

    use super::*;

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

    fn options() -> TestResult<(PiSdkOptions, pi_rs_ai::faux::FauxHandle)> {
        let faux = faux_provider(FauxOptions {
            api: Some("faux-sdk".into()),
            ..FauxOptions::default()
        });
        let model = faux.get_model(None).ok_or("no faux model")?.clone();
        let models = Models::new(ModelsOptions::default());
        models.set_provider(faux.provider.clone());
        let mut options = PiSdkOptions {
            model: Some(model),
            stream_fn: Some(pi_rs_agent::models_stream_fn(models)),
            ..PiSdkOptions::default()
        };
        options.cwd = "/tmp/pi-sdk".to_string();
        Ok((options, faux))
    }

    fn answer(text: &str) -> FauxStep {
        let mut message = faux_assistant_message(vec![faux_text(text)]);
        message.stop_reason = StopReason::Stop;
        message.into()
    }

    #[test]
    fn creates_session_with_defaults() -> TestResult {
        let (options, _faux) = options()?;
        let sdk = create_sdk_session(options)?;
        assert_eq!(sdk.session().cwd(), "/tmp/pi-sdk");
        assert!(!sdk.session().session_id().is_empty());
        sdk.close();
        Ok(())
    }

    #[test]
    fn requires_a_stream_function() {
        let result = create_sdk_session(PiSdkOptions::default());
        assert!(matches!(result, Err(SessionError::Extension(_))));
    }

    #[test]
    fn keeps_explicit_session_manager() -> TestResult {
        let (mut options, _faux) = options()?;
        let manager: Arc<dyn SessionManager> = Arc::new(InMemorySessionManager::new("/tmp/pi-sdk"));
        options.session_manager = Some(manager.clone());
        let sdk = create_sdk_session(options)?;
        assert_eq!(sdk.session().session_id(), manager.session_id());
        Ok(())
    }

    #[tokio::test]
    async fn prompt_uses_faux_provider_without_network() -> TestResult {
        let (options, faux) = options()?;
        faux.set_responses([answer("from sdk")]);
        let sdk = create_sdk_session(options)?;
        assert_eq!(
            sdk.prompt("hello", PromptOptions::default()).await?,
            PromptDisposition::Started
        );
        assert_eq!(faux.state.call_count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn subscribers_receive_session_events() -> TestResult {
        let (options, faux) = options()?;
        faux.set_responses([answer("subscribed")]);
        let sdk = create_sdk_session(options)?;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let id = sdk.subscribe(Arc::new(move |event: &AgentSessionEvent| {
            if let Ok(mut sink) = sink.lock() {
                sink.push(event.kind());
            }
        }));
        sdk.prompt("hi", PromptOptions::default()).await?;
        let kinds = seen
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        assert!(kinds.contains(&"agent_start"));
        assert!(sdk.unsubscribe(id));
        Ok(())
    }

    #[test]
    fn close_disposes_the_session() -> TestResult {
        let (options, _faux) = options()?;
        let sdk = create_sdk_session(options)?;
        sdk.close();
        sdk.close();
        assert!(!sdk.session().is_streaming());
        Ok(())
    }

    #[test]
    fn upstream_alias_creates_a_session() -> TestResult {
        let (options, _faux) = options()?;
        let sdk = create_agent_session(options)?;
        assert_eq!(sdk.session().stats().session_id, sdk.session().session_id());
        Ok(())
    }
}
