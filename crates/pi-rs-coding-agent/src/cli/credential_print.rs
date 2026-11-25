//! Resolve one usable credential through the normal request-auth path.

use std::path::Path;
use std::time::Duration;

use indexmap::IndexMap;
use pi_rs_agent::CancellationToken;
use pi_rs_ai::auth::{AuthOverrides, AuthType};
use pi_rs_ai::registry::RefreshOptions;

use super::auth_command::{Kind, Runtime, Selection, credential_value};
use crate::core::model_resolver::{
    ModelReferenceRequest, ModelReferenceResult, ThinkingSuffix, resolve_model_reference,
};

const DEFAULT_BEARER_TOKEN_MIN_EXPIRY: Duration = Duration::from_secs(30 * 60);
const RESOLUTION_FAILURE: &str = "Failed to resolve credential";

pub(super) async fn resolve(
    selection: Selection<'_>,
    kind: Kind,
    min_expiry: Option<Duration>,
    agent_dir: &Path,
    signal: &CancellationToken,
) -> Result<String, String> {
    let runtime = Runtime::new(agent_dir, false, false).map_err(|_| RESOLUTION_FAILURE)?;
    // Restore cached catalogs (if any) without fetching them or refreshing OAuth.
    runtime
        .models()
        .refresh(RefreshOptions {
            allow_network: false,
            signal: signal.clone(),
            ..RefreshOptions::default()
        })
        .await;
    resolve_with_runtime(&runtime, selection, kind, min_expiry, signal).await
}

async fn resolve_with_runtime(
    runtime: &Runtime,
    selection: Selection<'_>,
    kind: Kind,
    min_expiry: Option<Duration>,
    signal: &CancellationToken,
) -> Result<String, String> {
    let credential_types: IndexMap<_, _> = runtime
        .credentials
        .list(signal)
        .await
        .map_err(|_| RESOLUTION_FAILURE)?
        .into_iter()
        .map(|item| (item.provider_id, item.auth_type))
        .collect();
    let mut providers = Vec::new();
    if let Some(id) = selection.provider {
        let provider = runtime.registry.get_provider(id).ok_or_else(|| {
            format!("Unknown provider \"{id}\". Use --list-models to see available providers.")
        })?;
        let model = if selection.model.is_some() {
            Some(runtime.resolve_model(selection)?)
        } else {
            None
        };
        providers.push((provider.id, model));
    } else if let Some(text) = selection.model {
        let catalog = runtime.registry.list();
        for provider in runtime.models().get_providers() {
            if !credential_types.contains_key(&provider.id) {
                continue;
            }
            let request = ModelReferenceRequest {
                text,
                provider: Some(&provider.id),
                thinking_suffix: ThinkingSuffix::Strict,
                fuzzy: true,
            };
            if let ModelReferenceResult::Resolved { model, .. } =
                resolve_model_reference(&request, &catalog, &|_| true)
            {
                providers.push((provider.id, Some(model)));
            }
        }
        if providers.is_empty() {
            return Err(format!(
                "Model \"{text}\" not found. Use --list-models to see available models."
            ));
        }
    }
    let overrides = AuthOverrides {
        min_oauth_validity: (kind == Kind::BearerToken)
            .then_some(min_expiry.unwrap_or(DEFAULT_BEARER_TOKEN_MIN_EXPIRY)),
        signal: signal.clone(),
        ..AuthOverrides::default()
    };
    let accepts = |id: &str| {
        let oauth = credential_types.get(id) == Some(&AuthType::OAuth);
        match kind {
            Kind::ApiKey => !oauth,
            Kind::BearerToken => oauth,
            Kind::Check => false,
        }
    };
    let mut matches = Vec::new();
    for (id, model) in &providers {
        if accepts(id)
            && let Some(credential) = runtime
                .get_auth(id, model.as_ref(), &overrides)
                .await
                .map_err(|_| RESOLUTION_FAILURE)?
                .and_then(credential_value)
        {
            matches.push((id.as_str(), credential));
        }
    }
    match matches.len() {
        1 => matches
            .pop()
            .map(|(_, value)| value)
            .ok_or_else(|| RESOLUTION_FAILURE.into()),
        0 => {
            if let Some(id) = selection.provider.filter(|id| !accepts(id)) {
                if kind == Kind::ApiKey {
                    return Err(format!(
                        "Provider \"{id}\" is configured with OAuth, not an API key"
                    ));
                }
                return Err(format!(
                    "Provider \"{id}\" is not configured with an OAuth bearer token"
                ));
            }
            let label = if kind == Kind::ApiKey {
                "API key"
            } else {
                "OAuth bearer token"
            };
            Err(format!("No usable {label} is configured"))
        }
        _ => {
            let providers = matches
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "Multiple configured providers matched ({providers}). Specify --provider."
            ))
        }
    }
}
