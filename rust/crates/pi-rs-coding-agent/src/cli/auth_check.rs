//! Readiness checks never refresh catalogs, and `--no-refresh` never writes credentials.

use std::path::Path;

use pi_rs_agent::CancellationToken;
use pi_rs_ai::auth::{AuthOverrides, AuthType, Credential};
use serde::Serialize;

use super::auth_command::{Runtime, Selection, credential_value};

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Ready,
    NotReady,
    Invalid,
}

impl Status {
    fn code(self) -> u8 {
        match self {
            Self::Ready => 0,
            Self::NotReady => 1,
            Self::Invalid => 2,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NotReady => "not_ready",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Reason {
    ProviderNotFound,
    CredentialsNotConfigured,
    CredentialNotAvailable,
    InvalidState,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckResult {
    status: Status,
    provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<Reason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_type: Option<AuthType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credentials: Option<String>,
}

impl CheckResult {
    fn unavailable(provider: String, reason: Reason) -> Self {
        let status = if matches!(reason, Reason::InvalidState) {
            Status::Invalid
        } else {
            Status::NotReady
        };
        Self {
            status,
            provider,
            reason: Some(reason),
            auth_type: None,
            credentials: None,
        }
    }
}

async fn check(
    runtime: &Runtime,
    selection: Selection<'_>,
    emit_credentials: bool,
    no_refresh: bool,
) -> Result<CheckResult, String> {
    let signal = CancellationToken::new();
    // The auth-check runtime never performs the startup availability refresh;
    // selection does not use a cached authenticated-provider set here.
    let provider = if selection.model.is_some() {
        runtime.resolve_model(selection)?.base.provider
    } else {
        selection
            .provider
            .ok_or("Unable to resolve an auth provider")?
            .to_owned()
    };
    let unavailable = |reason| CheckResult::unavailable(provider.clone(), reason);
    if runtime.error().is_some() {
        return Ok(unavailable(Reason::InvalidState));
    }
    if runtime.registry.get_provider(&provider).is_none() {
        return Ok(unavailable(Reason::ProviderNotFound));
    }
    let checked = match runtime.models().check_auth(&provider, &signal).await {
        Ok(Some(auth)) => auth,
        Ok(None) => return Ok(unavailable(Reason::CredentialsNotConfigured)),
        Err(_) => return Ok(unavailable(Reason::InvalidState)),
    };
    let overrides = AuthOverrides {
        signal: signal.clone(),
        ..AuthOverrides::default()
    };
    let credential = if no_refresh {
        if emit_credentials {
            let saved = runtime
                .credentials
                .read(&provider, &signal)
                .await
                .map_err(|e| e.to_string())?;
            match saved {
                Some(Credential::OAuth(oauth)) => Some(oauth.access),
                _ => runtime
                    .get_auth(&provider, None, &overrides)
                    .await
                    .map_err(|e| e.to_string())?
                    .and_then(credential_value),
            }
        } else {
            None
        }
    } else {
        match runtime.get_auth(&provider, None, &overrides).await {
            Ok(Some(auth)) => emit_credentials.then(|| credential_value(auth)).flatten(),
            Ok(None) => return Ok(unavailable(Reason::CredentialsNotConfigured)),
            Err(_) => return Ok(unavailable(Reason::InvalidState)),
        }
    };
    if emit_credentials && credential.as_ref().is_none_or(String::is_empty) {
        return Ok(unavailable(Reason::CredentialNotAvailable));
    }
    Ok(CheckResult {
        status: Status::Ready,
        provider,
        reason: None,
        auth_type: Some(checked.auth_type),
        credentials: credential,
    })
}

pub(super) async fn run(
    selection: Selection<'_>,
    agent_dir: &Path,
    json: bool,
    credentials: bool,
    no_refresh: bool,
) -> u8 {
    let result = match Runtime::new(agent_dir, true, no_refresh) {
        Ok(runtime) => check(&runtime, selection, credentials, no_refresh).await,
        Err(error) => Err(error),
    };
    let result = result.unwrap_or_else(|_| {
        CheckResult::unavailable(
            selection
                .provider
                .or(selection.model)
                .unwrap_or_default()
                .to_owned(),
            Reason::InvalidState,
        )
    });
    if json {
        match serde_json::to_string(&result) {
            Ok(output) => println!("{output}"),
            Err(error) => {
                eprintln!("Error: {error}");
                return 2;
            }
        }
    } else {
        println!(
            "{}",
            result
                .credentials
                .as_deref()
                .unwrap_or_else(|| result.status.as_str())
        );
    }
    result.status.code()
}
