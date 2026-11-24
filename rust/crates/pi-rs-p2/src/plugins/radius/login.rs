//! Device-code login for the server's relay identity.

use std::{future::Future, path::Path, pin::Pin, sync::Arc};

use pi_rs_ai::{
    Error as AiError,
    auth::{AuthEvent, AuthInteraction, AuthPrompt, Credential, LoginOptions, OAuthAuth},
    providers::radius_provider,
};
use pi_rs_chord::{Error, Result};
use url::Url;
use uuid::Uuid;

use super::brain::{Config, ConfigFile};

pub fn gateway_from_env() -> Result<Url> {
    let value =
        std::env::var("PI_RADIUS_GATEWAY").unwrap_or_else(|_| "https://radius.pi.dev".into());
    let value = value.trim_end_matches('/');
    let url = if value.contains("://") {
        Url::parse(value)
    } else {
        Url::parse(&format!("https://{value}"))
    }
    .map_err(invalid)?;
    if !matches!(url.scheme(), "https" | "http")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error::Invalid("Invalid Radius gateway URL".into()));
    }
    Ok(url)
}

/// Sign in without a browser on the host. The caller may cancel through the supplied login options.
pub async fn login(
    directory: &Path,
    gateway: Url,
    on_code: impl Fn(&str, &str) + Send + Sync,
    options: &LoginOptions,
) -> Result<String> {
    let file = ConfigFile::new(directory);
    let credential = oauth(gateway.clone())?
        .login(&DeviceCode(on_code), options)
        .await
        .map_err(invalid)?;
    let gateway = gateway.as_str().trim_end_matches('/').to_owned();
    let mut config = file
        .read()
        .await?
        .filter(|old| old.gateway.trim_end_matches('/') == gateway)
        .unwrap_or_else(|| Config {
            gateway: gateway.clone(),
            server_id: Uuid::new_v4().to_string(),
            public_origin: None,
            shell_page: None,
            credential: Credential::OAuth(credential.clone()),
        });
    config.gateway = gateway;
    config.credential = Credential::OAuth(credential);
    file.write(&config).await?;
    Ok(config.server_id)
}

pub(super) fn oauth(gateway: Url) -> Result<Arc<dyn OAuthAuth>> {
    radius_provider("radius", "Radius", gateway)
        .map_err(invalid)?
        .auth
        .oauth
        .ok_or_else(|| Error::Invalid("The Radius provider has no OAuth support".into()))
}

struct DeviceCode<F>(F);

impl<F: Fn(&str, &str) + Send + Sync> AuthInteraction for DeviceCode<F> {
    fn prompt<'a, 'async_trait>(
        &'a self,
        prompt: AuthPrompt,
    ) -> Pin<Box<dyn Future<Output = pi_rs_ai::Result<String>> + Send + 'async_trait>>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            if prompt.signal.is_cancelled() {
                Err(AiError::Aborted)
            } else {
                Ok("device-code".into())
            }
        })
    }

    fn notify(&self, event: AuthEvent) {
        if let AuthEvent::DeviceCode {
            verification_uri,
            user_code,
            ..
        } = event
        {
            (self.0)(&verification_uri, &user_code);
        }
    }
}

pub(super) fn invalid(error: impl std::fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}
