use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use fancy_regex::Regex;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use pi_rs_agent::CancellationToken;
use pi_rs_ai::registry::{RefreshOptions, RefreshResult};
use pi_rs_ai::{Models, ModelsOptions};

pub fn get_model_search_text(id: &str, provider: &str, name: Option<&str>) -> String {
    let name = name.unwrap_or_default();
    let separator = if name.is_empty() { "" } else { " " };
    format!("{id} {provider} {provider}/{id} {provider} {id}{separator}{name}")
}

/// Keep provider names first so an exact provider/id outranks a proxy's namespaced ID.
pub fn get_model_selector_search_text(id: &str, provider: &str, name: Option<&str>) -> String {
    let name = name.unwrap_or_default();
    let separator = if name.is_empty() { "" } else { " " };
    format!("{provider} {provider}/{id} {provider} {id}{separator}{name}")
}

/// Suppress names which only restate the model ID.
pub fn get_distinct_model_name<'a>(id: &str, name: Option<&'a str>) -> Option<&'a str> {
    let name = name?.trim();
    if name.is_empty() {
        return None;
    }
    let lower_id = id.to_lowercase();
    let id_tokens = words(&lower_id).collect::<Vec<_>>();
    let compact_id = id_tokens.concat();
    if words(&name.to_lowercase()).all(|word| {
        id_tokens.contains(&word)
            || (word.encode_utf16().nth(1).is_some() && compact_id.contains(word))
    }) {
        None
    } else {
        Some(name)
    }
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    static WORD_CHARACTER: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]").ok());
    text.split(|ch: char| {
        if ch.is_ascii() {
            !ch.is_ascii_alphanumeric()
        } else {
            let mut bytes = [0; 4];
            !WORD_CHARACTER
                .as_ref()
                .and_then(|pattern| pattern.is_match(ch.encode_utf8(&mut bytes)).ok())
                .unwrap_or_else(|| ch.is_alphanumeric())
        }
    })
    .filter(|word| !word.is_empty())
}

type SharedRefresh = Shared<BoxFuture<'static, Arc<RefreshResult>>>;

struct ActiveRefresh {
    signal: CancellationToken,
    future: SharedRefresh,
}

impl Drop for ActiveRefresh {
    fn drop(&mut self) {
        self.signal.cancel();
    }
}

/// One refresh coordinator per model registry. Concurrent callers share the request and
/// independently cancel their own wait; dropping the final waiter drops the network operation.
pub struct ModelCatalogRefresh {
    models: Models,
    active: Mutex<[Weak<ActiveRefresh>; 2]>,
}

impl ModelCatalogRefresh {
    pub fn new(models: Models) -> Self {
        Self {
            models,
            active: Mutex::default(),
        }
    }

    /// Open the native registry using the same credentials and package catalogs as the runtime.
    pub fn from_agent_dir(agent_dir: &Path) -> Result<Self, String> {
        let models = Models::builtin(ModelsOptions {
            credentials: crate::cli::auth_command::credential_store(agent_dir),
            ..ModelsOptions::default()
        })
        .map_err(|error| error.to_string())?;
        crate::package_manager_cli::restore_model_catalogs(&models, agent_dir)?;
        let registry = crate::core::model_registry::ModelRegistry::new(
            models,
            Some(agent_dir.join("models.json")),
        );
        if let Some(error) = registry.get_error() {
            return Err(error.to_owned());
        }
        Ok(Self::new(registry.models().clone()))
    }

    pub fn models(&self) -> &Models {
        &self.models
    }

    pub async fn refresh(
        &self,
        allow_network: bool,
        signal: &CancellationToken,
    ) -> Result<Arc<RefreshResult>, String> {
        const CANCELLED: &str = "Model catalog refresh cancelled";
        if signal.is_cancelled() {
            return Err(CANCELLED.to_owned());
        }
        let slot = usize::from(allow_network);
        let active = {
            let mut active = self.active.lock().map_err(|error| error.to_string())?;
            match active[slot].upgrade() {
                Some(existing) => existing,
                None => {
                    let signal = CancellationToken::new();
                    let models = self.models.clone();
                    let options = RefreshOptions {
                        allow_network,
                        signal: signal.clone(),
                        ..RefreshOptions::default()
                    };
                    let future = async move { Arc::new(models.refresh(options).await) }
                        .boxed()
                        .shared();
                    let existing = Arc::new(ActiveRefresh { signal, future });
                    active[slot] = Arc::downgrade(&existing);
                    existing
                }
            }
        };
        let result = tokio::select! {
            biased;
            _ = signal.cancelled() => return Err(CANCELLED.to_owned()),
            result = active.future.clone() => result,
        };
        let mut slots = self.active.lock().map_err(|error| error.to_string())?;
        if slots[slot].as_ptr() == Arc::as_ptr(&active) {
            slots[slot] = Weak::new();
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use pi_rs_ai::models_store::ModelsStoreEntry;
    use pi_rs_ai::registry::{ModelSource, ModelSourceRequest, Provider};
    use tokio::sync::Notify;

    use super::*;

    #[test]
    fn search_and_distinct_labels_match_provider_and_name_rules() {
        assert_eq!(
            get_model_search_text("a/b", "proxy", Some("Alias")),
            "a/b proxy proxy/a/b proxy a/b Alias"
        );
        assert_eq!(
            get_model_selector_search_text("a/b", "proxy", None),
            "proxy proxy/a/b proxy a/b"
        );
        assert_eq!(
            get_distinct_model_name(
                "eu.anthropic.claude-sonnet-4-5-20250929-v1:0",
                Some(" Claude Sonnet 4.5 (EU) ")
            ),
            None
        );
        assert_eq!(
            get_distinct_model_name("mistral-small-2603", Some("Mistral Small 6")),
            Some("Mistral Small 6")
        );
        assert_eq!(
            get_distinct_model_name("minimax/minimax-m3", Some(" MiniMax M3 [parasail] ")),
            Some("MiniMax M3 [parasail]")
        );
        assert_eq!(
            get_distinct_model_name("model", Some("model\u{0345}")),
            None
        );
    }

    #[derive(Default)]
    struct Source {
        started: Notify,
        release: Notify,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ModelSource for Source {
        async fn refresh(
            &self,
            request: ModelSourceRequest<'_>,
        ) -> pi_rs_ai::Result<Option<ModelsStoreEntry>> {
            assert!(!request.allow_network);
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.started.notify_one();
            self.release.notified().await;
            Ok(None)
        }
    }

    #[tokio::test]
    async fn a_cancelled_waiter_cannot_cancel_another_waiter() {
        let models = Models::default();
        let source = Arc::new(Source::default());
        let mut provider = Provider::new("test", "Test");
        provider.source = Some(source.clone());
        models.set_provider(provider);
        let coordinator = ModelCatalogRefresh::new(models);
        let first = CancellationToken::new();
        let second = CancellationToken::new();
        let (cancelled, kept, ()) = tokio::join!(
            coordinator.refresh(false, &first),
            coordinator.refresh(false, &second),
            async {
                source.started.notified().await;
                first.cancel();
                source.release.notify_one();
            },
        );
        assert!(cancelled.is_err());
        assert!(kept.is_ok());
        assert_eq!(source.calls.load(Ordering::Relaxed), 1);
    }
}
