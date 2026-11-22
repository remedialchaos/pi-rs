//! Cwd-bound runtime around an [`AgentSession`].
//!
//! Upstream's `agent-session-runtime.ts` owns the current session plus the services that were
//! created for its effective cwd. A `/new`, `/resume`, `/fork`, or import tears the old session
//! down (settle the turn, emit `session_shutdown`, dispose) and then applies a freshly created
//! runtime through a factory. Diagnostics are collected from runtime creation and reported once
//! per unique type/message pair.

use std::{
    collections::HashSet,
    fmt::{Display, Formatter},
    sync::{Arc, Mutex},
};

use futures_util::future::BoxFuture;

use super::agent_session::{
    AgentSession, AuthStorage, Compactor, ExtensionRunner, ForkPosition, InMemorySessionManager,
    ModelRegistry, Result, SessionError, SessionManager, SessionReason, SessionStats,
    SettingsStore,
};

/// Severity of a non-fatal runtime diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

impl DiagnosticLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeDiagnostic {
    pub level: DiagnosticLevel,
    pub message: String,
}

impl RuntimeDiagnostic {
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            level: DiagnosticLevel::Warning,
            message: message.into(),
        }
    }

    fn key(&self) -> String {
        format!("{}\0{}", self.level.as_str(), self.message)
    }
}

/// Services rebuilt whenever the effective session cwd changes.
pub struct RuntimeServices {
    pub cwd: String,
    pub agent_dir: Option<String>,
    pub settings: Arc<dyn SettingsStore>,
    pub auth: Arc<dyn AuthStorage>,
    pub models: Arc<dyn ModelRegistry>,
    pub compactor: Arc<dyn Compactor>,
    pub extensions: Arc<dyn ExtensionRunner>,
}

/// Input to the runtime factory.
pub struct RuntimeTarget {
    pub cwd: String,
    pub agent_dir: Option<String>,
    pub session_manager: Arc<dyn SessionManager>,
    pub reason: SessionReason,
}

/// Result of creating a runtime.
pub struct CreateAgentSessionRuntimeResult {
    pub session: AgentSession,
    pub services: RuntimeServices,
    pub diagnostics: Vec<RuntimeDiagnostic>,
    pub model_fallback_message: Option<String>,
}

pub type CreateAgentSessionRuntimeFactory = Arc<
    dyn Fn(RuntimeTarget) -> BoxFuture<'static, Result<CreateAgentSessionRuntimeResult>>
        + Send
        + Sync,
>;

type RebindSession = Arc<dyn Fn(&AgentSession) -> BoxFuture<'static, Result<()>> + Send + Sync>;
type BeforeInvalidate = Arc<dyn Fn() + Send + Sync>;

/// Error returned when a session file referenced by an import does not exist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionImportFileNotFoundError {
    pub file_path: String,
}

impl Display for SessionImportFileNotFoundError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "File not found: {}", self.file_path)
    }
}

impl std::error::Error for SessionImportFileNotFoundError {}

/// Owns the active session and its services, replacing both atomically.
pub struct AgentSessionRuntime {
    session: AgentSession,
    services: RuntimeServices,
    factory: CreateAgentSessionRuntimeFactory,
    diagnostics: Vec<RuntimeDiagnostic>,
    model_fallback_message: Option<String>,
    reported: Mutex<HashSet<String>>,
    rebind_session: Option<RebindSession>,
    before_invalidate: Option<BeforeInvalidate>,
}

impl AgentSessionRuntime {
    pub fn new(
        session: AgentSession,
        services: RuntimeServices,
        factory: CreateAgentSessionRuntimeFactory,
        diagnostics: Vec<RuntimeDiagnostic>,
        model_fallback_message: Option<String>,
    ) -> Self {
        Self {
            session,
            services,
            factory,
            diagnostics,
            model_fallback_message,
            reported: Mutex::new(HashSet::new()),
            rebind_session: None,
            before_invalidate: None,
        }
    }

    pub fn session(&self) -> &AgentSession {
        &self.session
    }

    pub fn services(&self) -> &RuntimeServices {
        &self.services
    }

    pub fn cwd(&self) -> &str {
        &self.services.cwd
    }

    pub fn diagnostics(&self) -> &[RuntimeDiagnostic] {
        &self.diagnostics
    }

    pub fn model_fallback_message(&self) -> Option<&str> {
        self.model_fallback_message.as_deref()
    }

    /// Return diagnostics not reported yet and mark them reported, so a settings warning is not
    /// repeated after every session replacement.
    pub fn take_unreported_diagnostics(&self) -> Vec<RuntimeDiagnostic> {
        let Ok(mut reported) = self.reported.lock() else {
            return Vec::new();
        };
        self.diagnostics
            .iter()
            .filter(|diagnostic| {
                let key = diagnostic.key();
                if reported.contains(&key) {
                    false
                } else {
                    reported.insert(key);
                    true
                }
            })
            .cloned()
            .collect()
    }

    pub fn set_rebind_session(&mut self, callback: Option<RebindSession>) {
        self.rebind_session = callback;
    }

    pub fn set_before_session_invalidate(&mut self, callback: Option<BeforeInvalidate>) {
        self.before_invalidate = callback;
    }

    async fn teardown(&self, reason: SessionReason) {
        self.session.abort().await;
        let value = match reason {
            SessionReason::Reload => "reload",
            SessionReason::Quit => "quit",
            _ => "other",
        };
        self.session
            .emit_runtime_event(serde_json::json!({ "type": "session_shutdown", "reason": value }));
        if let Some(before) = &self.before_invalidate {
            before();
        }
        self.session.dispose();
    }

    fn apply(&mut self, result: CreateAgentSessionRuntimeResult) {
        self.session = result.session;
        self.services = result.services;
        self.diagnostics = result.diagnostics;
        self.model_fallback_message = result.model_fallback_message;
    }

    async fn replace(&mut self, target: RuntimeTarget) -> Result<()> {
        if target.session_manager.cwd() != target.cwd {
            return Err(SessionError::Extension(format!(
                "Session cwd {} does not match runtime cwd {}",
                target.session_manager.cwd(),
                target.cwd
            )));
        }
        self.teardown(target.reason).await;
        let result = (self.factory)(target).await?;
        self.apply(result);
        if let Some(rebind) = &self.rebind_session {
            rebind(&self.session).await?;
        }
        Ok(())
    }

    /// Resume a different session file (represented by its session manager).
    pub async fn switch_session(&mut self, manager: Arc<dyn SessionManager>) -> Result<()> {
        let target = RuntimeTarget {
            cwd: manager.cwd(),
            agent_dir: self.services.agent_dir.clone(),
            session_manager: manager,
            reason: SessionReason::Resume,
        };
        self.replace(target).await
    }

    /// Start a new session under the current cwd.
    pub async fn new_session(&mut self) -> Result<()> {
        let cwd = self.cwd().to_string();
        let manager: Arc<dyn SessionManager> = Arc::new(InMemorySessionManager::new(cwd.clone()));
        let target = RuntimeTarget {
            cwd,
            agent_dir: self.services.agent_dir.clone(),
            session_manager: manager,
            reason: SessionReason::New,
        };
        self.replace(target).await
    }

    /// Fork at an entry of the current session tree.
    pub async fn fork(&mut self, entry_id: &str, position: ForkPosition) -> Result<()> {
        let current = self.session.session_manager();
        let cwd = current.cwd();
        current.fork(entry_id, position)?;
        let target = RuntimeTarget {
            cwd,
            agent_dir: self.services.agent_dir.clone(),
            session_manager: current,
            reason: SessionReason::Fork,
        };
        self.replace(target).await
    }

    pub fn stats(&self) -> SessionStats {
        self.session.stats()
    }

    /// Dispose the current session for process exit.
    pub async fn dispose(&mut self) {
        self.teardown(SessionReason::Quit).await;
    }
}

/// Create the initial runtime from a factory and target.
pub async fn create_agent_session_runtime(
    factory: CreateAgentSessionRuntimeFactory,
    target: RuntimeTarget,
) -> Result<AgentSessionRuntime> {
    if target.session_manager.cwd() != target.cwd {
        return Err(SessionError::Extension(format!(
            "Session cwd {} does not match runtime cwd {}",
            target.session_manager.cwd(),
            target.cwd
        )));
    }
    let result = factory(target).await?;
    Ok(AgentSessionRuntime::new(
        result.session,
        result.services,
        factory,
        result.diagnostics,
        result.model_fallback_message,
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use pi_rs_agent::{Agent, AgentInitialState, AgentOptions};
    use pi_rs_ai::{
        Models, ModelsOptions, ThinkingLevel,
        faux::{FauxOptions, faux_provider},
    };

    use super::*;
    use crate::core::agent_session::{
        InMemorySettings, NoopExtensionRunner, SessionStatsSnapshot, StaticAuth, TailCompactor,
    };

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

    struct StubManager {
        cwd: String,
        id: String,
        stats: Arc<Mutex<SessionStatsSnapshot>>,
    }

    impl StubManager {
        fn new(cwd: &str, id: &str) -> Self {
            Self {
                cwd: cwd.to_string(),
                id: id.to_string(),
                stats: Arc::new(Mutex::new(SessionStatsSnapshot::default())),
            }
        }
    }

    impl SessionManager for StubManager {
        fn session_id(&self) -> String {
            self.id.clone()
        }
        fn session_file(&self) -> Option<String> {
            None
        }
        fn cwd(&self) -> String {
            self.cwd.clone()
        }
        fn messages(&self) -> Vec<pi_rs_agent::AgentMessage> {
            Vec::new()
        }
        fn append_message(&self, _message: &pi_rs_agent::AgentMessage) -> String {
            self.id.clone()
        }
        fn append_model_change(&self, _provider: &str, _model_id: &str) {}
        fn append_thinking_level_change(&self, _level: ThinkingLevel) {}
        fn append_compaction(
            &self,
            _summary: &str,
            _first_kept_entry_id: Option<String>,
        ) -> String {
            self.id.clone()
        }
        fn entries(&self) -> Vec<crate::core::agent_session::SessionEntry> {
            Vec::new()
        }
        fn navigate(&self, _entry_id: &str) -> Result<()> {
            Ok(())
        }
        fn fork(&self, _entry_id: &str, _position: ForkPosition) -> Result<()> {
            Ok(())
        }
        fn stats(&self) -> SessionStatsSnapshot {
            self.stats
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }
    }

    fn factory(counter: Arc<AtomicUsize>) -> CreateAgentSessionRuntimeFactory {
        Arc::new(move |target| {
            let counter = counter.clone();
            Box::pin(async move {
                let faux = faux_provider(FauxOptions {
                    api: Some("faux-runtime".into()),
                    ..FauxOptions::default()
                });
                let model = faux.models[0].clone();
                let models = Models::new(ModelsOptions::default());
                models.set_provider(faux.provider.clone());
                let agent = Agent::new(AgentOptions {
                    initial_state: AgentInitialState {
                        model: Some(model.clone()),
                        thinking_level: Some(ThinkingLevel::Medium),
                        ..AgentInitialState::default()
                    },
                    stream_fn: Some(pi_rs_agent::models_stream_fn(models)),
                    ..AgentOptions::default()
                })?;
                let auth: Arc<dyn AuthStorage> = Arc::new(StaticAuth::allow_all());
                let registry: Arc<dyn ModelRegistry> =
                    Arc::new(crate::core::agent_session::InMemoryModelRegistry::new(
                        vec![model],
                        auth.clone(),
                    ));
                let settings: Arc<dyn SettingsStore> = Arc::new(InMemorySettings::default());
                session_count(&counter);
                let session = AgentSession::new(crate::core::agent_session::AgentSessionConfig {
                    agent,
                    session_manager: target.session_manager,
                    settings: settings.clone(),
                    auth,
                    models: registry,
                    compactor: Arc::new(TailCompactor { keep_recent: 1 }),
                    extensions: Arc::new(NoopExtensionRunner),
                    cwd: target.cwd.clone(),
                    retry_prompt: None,
                })?;
                Ok(CreateAgentSessionRuntimeResult {
                    session,
                    services: RuntimeServices {
                        cwd: target.cwd,
                        agent_dir: target.agent_dir,
                        settings,
                        auth: Arc::new(StaticAuth::allow_all()),
                        models: Arc::new(crate::core::agent_session::InMemoryModelRegistry::new(
                            Vec::new(),
                            Arc::new(StaticAuth::allow_all()),
                        )),
                        compactor: Arc::new(TailCompactor { keep_recent: 1 }),
                        extensions: Arc::new(NoopExtensionRunner),
                    },
                    diagnostics: vec![RuntimeDiagnostic::warning("settings fallback")],
                    model_fallback_message: Some("using fallback model".to_string()),
                })
            })
        })
    }

    fn session_count(counter: &AtomicUsize) -> usize {
        counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    async fn runtime() -> TestResult<(AgentSessionRuntime, Arc<AtomicUsize>)> {
        let counter = Arc::new(AtomicUsize::new(0));
        let target = RuntimeTarget {
            cwd: "/tmp/pi-runtime".to_string(),
            agent_dir: Some("/tmp/pi-runtime-agent".to_string()),
            session_manager: Arc::new(StubManager::new("/tmp/pi-runtime", "session-1")),
            reason: SessionReason::Startup,
        };
        let runtime = create_agent_session_runtime(factory(counter.clone()), target).await?;
        Ok((runtime, counter))
    }

    #[tokio::test]
    async fn reports_each_diagnostic_once() -> TestResult {
        let (runtime, _) = runtime().await?;
        assert_eq!(runtime.take_unreported_diagnostics().len(), 1);
        assert!(runtime.take_unreported_diagnostics().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn exposes_services_and_fallback_message() -> TestResult {
        let (runtime, _) = runtime().await?;
        assert_eq!(runtime.cwd(), "/tmp/pi-runtime");
        assert_eq!(
            runtime.services().agent_dir.as_deref(),
            Some("/tmp/pi-runtime-agent")
        );
        assert_eq!(
            runtime.model_fallback_message(),
            Some("using fallback model")
        );
        Ok(())
    }

    #[tokio::test]
    async fn rejects_cwd_mismatch() -> TestResult {
        let counter = Arc::new(AtomicUsize::new(0));
        let target = RuntimeTarget {
            cwd: "/tmp/other".to_string(),
            agent_dir: None,
            session_manager: Arc::new(StubManager::new("/tmp/pi-runtime", "session-1")),
            reason: SessionReason::Startup,
        };
        assert!(
            create_agent_session_runtime(factory(counter), target)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn switch_session_replaces_session_and_services() -> TestResult {
        let (mut runtime, counter) = runtime().await?;
        let old_session = runtime.session().clone();
        let manager: Arc<dyn SessionManager> =
            Arc::new(StubManager::new("/tmp/pi-runtime", "session-2"));
        runtime.switch_session(manager).await?;
        assert_eq!(counter.load(Ordering::Relaxed), 2);
        assert_ne!(runtime.session().session_id(), old_session.session_id());
        assert_eq!(runtime.session().session_id(), "session-2");
        Ok(())
    }

    #[tokio::test]
    async fn new_session_uses_a_fresh_manager() -> TestResult {
        let (mut runtime, counter) = runtime().await?;
        runtime.new_session().await?;
        assert_eq!(counter.load(Ordering::Relaxed), 2);
        assert!(runtime.session().messages().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn fork_replaces_the_runtime() -> TestResult {
        let (mut runtime, counter) = runtime().await?;
        runtime.fork("entry-1", ForkPosition::At).await?;
        assert_eq!(counter.load(Ordering::Relaxed), 2);
        Ok(())
    }

    #[tokio::test]
    async fn rebind_and_invalidate_callbacks_run_on_replacement() -> TestResult {
        let (mut runtime, _) = runtime().await?;
        let rebound = Arc::new(AtomicUsize::new(0));
        let invalidated = Arc::new(AtomicUsize::new(0));
        let observed = rebound.clone();
        runtime.set_rebind_session(Some(Arc::new(move |_session| {
            let observed = observed.clone();
            Box::pin(async move {
                observed.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        })));
        let observed = invalidated.clone();
        runtime.set_before_session_invalidate(Some(Arc::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        })));
        runtime.new_session().await?;
        assert_eq!(rebound.load(Ordering::Relaxed), 1);
        assert_eq!(invalidated.load(Ordering::Relaxed), 1);
        Ok(())
    }

    #[test]
    fn import_not_found_error_has_stable_message() {
        let error = SessionImportFileNotFoundError {
            file_path: "/tmp/missing.jsonl".to_string(),
        };
        assert_eq!(error.to_string(), "File not found: /tmp/missing.jsonl");
    }
}
