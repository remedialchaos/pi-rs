//! Dispatch `pi auth` before session, extension, and project setup.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;
use pi_rs_agent::CancellationToken;
use pi_rs_ai::auth::{
    ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthInteraction, AuthOverrides,
    AuthPrompt, AuthResult, AuthType, Credential, CredentialInfo, CredentialStore, LoginOptions,
    ModelAuth, Mutation, OAuthAuth, OAuthCredential, PromptKind,
};
use pi_rs_ai::models_store::{ModelsStore, ModelsStoreEntry};
use pi_rs_ai::providers::radius_provider;
use pi_rs_ai::registry::RefreshOptions;
use pi_rs_ai::{Error, Model, Models, ModelsOptions, Provider, Result as AiResult};
use serde::{Serialize, de::DeserializeOwned};

use super::args::{Args, parse_args};
use super::{auth_check, auth_login, auth_logout, credential_print};
use crate::core::model_registry::{MODELS_JSON_FILE, ModelRegistry};
use crate::core::model_resolver::{CliModelOptions, resolve_cli_model};
use crate::core::resolve_config_value::{
    get_config_value_env_var_names, is_command_config_value, resolve_config_value_str_with_env,
};
use crate::lockfile::{LockGuard, LockOptions, lock};
use crate::utils::text::strip_bom;

const HELP: &str = "Usage:
  pi auth login <provider> [--api-key] [--no-browser]
  pi auth logout <provider>
  pi auth print-api-key [--provider <provider>] [--model <model>]
  pi auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]
  pi auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]

Log in to and out of model providers, print their credentials for external clients, or check that they are ready.
Credentials are stored in ~/.pi/agent/auth.json.

Commands:
  login <provider>        Sign in with an account in the browser, or save an API key, then exit
  logout <provider>       Remove the stored credentials; environment variables and models.json stay
  print-api-key           Print the resolved API key
  print-bearer-token      Print a resolved OAuth bearer token (refreshes if expired)
  check                   Print ready, not_ready, or invalid; exit with 0, 1, or 2

Options for print-api-key, print-bearer-token, and check (at least one is required):
  --provider <provider>   Resolve credentials for a provider
  --model <model>         Resolve credentials from a model

Options for login:
  --api-key               Save an API key instead of signing in with an account
  --no-browser            Do not open a browser; use another sign-in method, e.g. device code

Other options:
  --min-expiry <duration> Minimum remaining token lifetime for print-bearer-token, e.g. 30m or 1h
  --json                  Print the check result as JSON
  --credentials           Emit the credential from check, or include it in JSON output
  --no-refresh            Do not refresh expired OAuth credentials during check";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Check,
    ApiKey,
    BearerToken,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Check => "auth check",
            Self::ApiKey => "auth print-api-key",
            Self::BearerToken => "auth print-bearer-token",
        }
    }

    fn usage(self) -> &'static str {
        match self {
            Self::Check => {
                "pi auth check --provider <provider> [--json] [--credentials] [--no-refresh]"
            }
            Self::ApiKey => "pi auth print-api-key --provider <provider> [--model <model>]",
            Self::BearerToken => {
                "pi auth print-bearer-token --provider <provider> [--model <model>] [--min-expiry <duration>]"
            }
        }
    }

    fn error_code(self) -> u8 {
        if self == Self::Check { 2 } else { 1 }
    }
}

struct Command {
    kind: Kind,
    args: Args,
    json: bool,
    credentials: bool,
    no_refresh: bool,
    min_expiry: Option<Duration>,
}

fn parse_command(name: &str, args: &[String]) -> Result<Command, String> {
    let kind = match name {
        "check" => Kind::Check,
        "print-api-key" => Kind::ApiKey,
        "print-bearer-token" => Kind::BearerToken,
        _ => {
            return Err(format!(
                "Unknown auth command \"{name}\". Use \"pi auth login\", \"pi auth logout\", \"pi auth print-api-key\", \"pi auth print-bearer-token\", or \"pi auth check\"."
            ));
        }
    };
    let mut command_args = Vec::with_capacity(args.len());
    let (mut json, mut credentials, mut no_refresh, mut min_expiry) = (false, false, false, None);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--min-expiry" => {
                if kind != Kind::BearerToken {
                    return Err("--min-expiry is only supported by print-bearer-token".into());
                }
                min_expiry = Some(
                    rest.next()
                        .and_then(|arg| parse_duration(arg))
                        .ok_or("--min-expiry must use a duration such as 30m or 1h")?,
                );
            }
            "--json" | "--credentials" | "--no-refresh" => {
                if kind != Kind::Check {
                    return Err(format!("{arg} is only supported by auth check"));
                }
                match arg.as_str() {
                    "--json" => json = true,
                    "--credentials" => credentials = true,
                    _ => no_refresh = true,
                }
            }
            _ => command_args.push(arg.clone()),
        }
    }
    Ok(Command {
        kind,
        args: parse_args(&command_args),
        json,
        credentials,
        no_refresh,
        min_expiry,
    })
}

fn parse_duration(value: &str) -> Option<Duration> {
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let (amount, unit) = value.split_at(digits);
    if !["ms", "s", "m", "h"]
        .iter()
        .any(|candidate| unit.eq_ignore_ascii_case(candidate))
        && !matches!(unit, "ſ" | "mſ" | "Mſ")
    {
        return None;
    }
    // The original recognizes suffixes case-insensitively, but selects units
    // from the original capture, defaulting to hours for non-lowercase suffixes.
    let multiplier = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        _ => 3_600_000,
    };
    let amount = amount.parse::<u64>().unwrap_or(u64::MAX);
    Some(Duration::from_millis(amount.saturating_mul(multiplier)))
}

#[derive(Clone, Copy)]
pub(super) struct Selection<'a> {
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub thinking: Option<pi_rs_ai::ThinkingLevel>,
}

fn selection(args: &Args, kind: Kind) -> Result<Selection<'_>, String> {
    let provider = args
        .provider
        .as_deref()
        .map(js_trim)
        .filter(|value| !value.is_empty());
    let model = args
        .model
        .as_deref()
        .map(js_trim)
        .filter(|value| !value.is_empty());
    if args.api_key.is_some() || !args.messages.is_empty() || !args.file_args.is_empty() {
        return Err("Auth commands only accept --provider and --model".into());
    }
    if provider.is_none() && model.is_none() {
        let prefix = if kind == Kind::Check {
            "Auth checks require"
        } else {
            "Credential printing requires"
        };
        return Err(format!("{prefix} --provider <provider> or --model <model>"));
    }
    Ok(Selection {
        provider,
        model,
        thinking: args.thinking,
    })
}

pub(super) fn js_trim(value: &str) -> &str {
    value.trim_matches(js_whitespace)
}

fn js_whitespace(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

/// Handles `auth` arguments (without the executable name) and returns their exit code.
/// Returns `None` if the first argument is not `auth`.
pub async fn run(args: &[String], cwd: &Path, agent_dir: &Path) -> Option<u8> {
    let [head, rest @ ..] = args else { return None };
    if head != "auth" {
        return None;
    }
    let Some((name, args)) = rest.split_first() else {
        println!("{HELP}");
        return Some(0);
    };
    if name == "help"
        || rest
            .iter()
            .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!("{HELP}");
        return Some(0);
    }
    let offline = args.iter().any(|arg| arg == "--offline")
        || crate::config::is_truthy_env_flag("PI_OFFLINE");
    if matches!(name.as_str(), "login" | "logout") {
        let args: Vec<&str> = args
            .iter()
            .map(String::as_str)
            .filter(|arg| *arg != "--offline")
            .collect();
        return Some(if name == "login" {
            auth_login::run(&args, cwd, agent_dir, offline).await
        } else {
            auth_logout::run(&args, agent_dir).await
        });
    }
    let command = match parse_command(name, args) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("Error: {error}");
            return Some(1);
        }
    };
    if let Some(option) = command.args.unknown_flags.keys().next() {
        eprintln!("Unknown option --{option} for \"{}\".", command.kind.name());
        eprintln!("Use \"pi --help\" or \"{}\".", command.kind.usage());
        return Some(1);
    }
    if !command.args.diagnostics.is_empty() {
        let message = command
            .args
            .diagnostics
            .iter()
            .map(|item| item.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("Error: {message}");
        return Some(command.kind.error_code());
    }
    let selected = match selection(&command.args, command.kind) {
        Ok(selection) => selection,
        Err(error) => {
            eprintln!("Error: {error}");
            return Some(command.kind.error_code());
        }
    };
    Some(if command.kind == Kind::Check {
        auth_check::run(
            selected,
            agent_dir,
            command.json,
            command.credentials,
            command.no_refresh,
        )
        .await
    } else {
        let signal = deadline();
        match credential_print::resolve(
            selected,
            command.kind,
            command.min_expiry,
            agent_dir,
            &signal,
        )
        .await
        {
            Ok(credential) => {
                println!("{credential}");
                0
            }
            Err(error) => {
                eprintln!("Error: {error}");
                1
            }
        }
    })
}

pub(super) fn deadline() -> CancellationToken {
    let signal = CancellationToken::new();
    let timeout = signal.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        timeout.cancel();
    });
    signal
}

pub(super) fn find_provider<'a>(
    providers: &'a [Provider],
    reference: &str,
) -> Option<&'a Provider> {
    let reference = js_trim(reference);
    if reference.is_empty() {
        return None;
    }
    let normalized = (!reference.is_ascii()).then(|| reference.to_lowercase());
    let matches = |value: &str| {
        value.eq_ignore_ascii_case(reference)
            || normalized.as_ref().map_or_else(
                || !value.is_ascii() && value.to_lowercase().eq_ignore_ascii_case(reference),
                |normalized| value.to_lowercase() == *normalized,
            )
    };
    providers
        .iter()
        .find(|provider| matches(&provider.id) || matches(&provider.name))
}

pub(super) fn credential_value(result: AuthResult) -> Option<String> {
    if let Some(key) = result.auth.api_key.filter(|key| !key.is_empty()) {
        return Some(key);
    }
    let headers = result.auth.headers?;
    let (_, header) = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))?;
    let header = header.as_deref()?;
    let split = header.find(js_whitespace)?;
    let (scheme, whitespace_and_token) = header.split_at(split);
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    // Like `\s+(.+)`, leave one whitespace character when there is no non-space token.
    let mut token = whitespace_and_token.trim_start_matches(js_whitespace);
    if token.is_empty() {
        let last = whitespace_and_token.char_indices().next_back()?.0;
        if last == 0 {
            return None;
        }
        token = &whitespace_and_token[last..];
    }
    (!token.contains(['\r', '\n', '\u{2028}', '\u{2029}'])).then(|| token.to_owned())
}

pub(crate) fn auth_providers(agent_dir: &Path) -> Result<Vec<Provider>, String> {
    Ok(Runtime::new(agent_dir, false, true)?
        .models()
        .get_providers())
}

pub(super) struct Runtime {
    pub registry: ModelRegistry,
    pub credentials: Arc<dyn CredentialStore>,
    composition_error: Option<String>,
}

impl Runtime {
    pub fn new(agent_dir: &Path, check: bool, read_only: bool) -> Result<Self, String> {
        let file = AuthFile {
            path: agent_dir.join("auth.json"),
            read_only,
        };
        if !read_only {
            // Match writable AuthStorage creation even when no credentials are configured.
            prepare_file(&file.path).map_err(|error| error.to_string())?;
        }
        let credentials = Arc::new(file);
        let mut options = ModelsOptions {
            credentials: credentials.clone(),
            ..ModelsOptions::default()
        };
        if !check {
            options.store = Arc::new(CatalogFile(agent_dir.join("models-store.json")));
        }
        let models = Models::builtin(options).map_err(|error| error.to_string())?;
        if !check {
            // An invalid or unavailable local cache does not block credential commands.
            let _ = crate::package_manager_cli::restore_model_catalogs(&models, agent_dir);
        }
        let mut runtime = Self {
            registry: ModelRegistry::new(models, Some(agent_dir.join(MODELS_JSON_FILE))),
            credentials,
            composition_error: None,
        };
        runtime.apply_auth_config();
        Ok(runtime)
    }

    fn apply_auth_config(&mut self) {
        for (id, config) in &self.registry.config().providers {
            let Some(mut provider) = self.registry.get_provider(id) else {
                continue;
            };
            if config.oauth.as_deref() == Some("radius")
                && let Some(url) = &config.base_url
            {
                let base = url.trim_end_matches('/');
                let gateway = base.strip_suffix("/v1").unwrap_or(base);
                match url::Url::parse(gateway)
                    .map_err(Error::from)
                    .and_then(|url| radius_provider(id, &provider.name, url))
                {
                    Ok(radius) => {
                        provider.auth = radius.auth;
                        provider.source = radius.source;
                    }
                    Err(error) => {
                        self.composition_error = Some(error.to_string());
                        continue;
                    }
                }
            }
            if provider.auth.api_key.is_some()
                || config.api_key.is_some()
                || provider.auth.oauth.is_none()
            {
                provider.auth.api_key = Some(Arc::new(ConfigAuth {
                    origin: provider.auth.api_key.take(),
                    key: config.api_key.clone(),
                    headers: ConfigHeaders::new(id, config.headers.clone(), config.auth_header),
                }));
            }
            if let Some(oauth) = provider.auth.oauth.take() {
                provider.auth.oauth = Some(Arc::new(ConfigOAuth {
                    origin: oauth,
                    headers: ConfigHeaders::new(id, config.headers.clone(), config.auth_header),
                }));
            }
            self.models().set_provider(provider);
        }
    }

    pub fn models(&self) -> &Models {
        self.registry.models()
    }

    pub fn error(&self) -> Option<&str> {
        self.registry
            .get_error()
            .or(self.composition_error.as_deref())
    }

    pub fn resolve_model(&self, selection: Selection<'_>) -> Result<Model, String> {
        let resolved = resolve_cli_model(&CliModelOptions {
            cli_provider: selection.provider,
            cli_model: selection.model,
            cli_thinking: selection.thinking,
            inline_api_key: false,
            models: &self.registry.list(),
            has_configured_auth: &|_| false,
        });
        resolved.error.map_or_else(
            || {
                resolved.model.ok_or_else(|| {
                    format!(
                        "Unable to resolve model \"{}\"",
                        selection.model.unwrap_or_default()
                    )
                })
            },
            Err,
        )
    }

    pub async fn get_auth(
        &self,
        provider: &str,
        model: Option<&Model>,
        overrides: &AuthOverrides,
    ) -> AiResult<Option<AuthResult>> {
        if let Some(model) = model {
            self.models().get_model_auth(model, overrides).await
        } else {
            self.models().get_auth(provider, overrides).await
        }
    }

    pub async fn synchronize(&self, provider: &str, signal: &CancellationToken) -> AiResult<()> {
        if signal.is_cancelled() {
            return Err(Error::Aborted);
        }
        let mut refreshed = self
            .models()
            .refresh(RefreshOptions {
                providers: Some(vec![provider.to_owned()]),
                allow_network: false,
                signal: signal.clone(),
            })
            .await;
        if let Some(error) = refreshed.errors.shift_remove(provider) {
            return Err(error);
        }
        self.models().check_auth(provider, signal).await?;
        Ok(())
    }
}

/// File-backed credentials with the coding agent's upstream-compatible directory lock.
pub fn credential_store(agent_dir: &Path) -> Arc<dyn CredentialStore> {
    Arc::new(AuthFile {
        path: agent_dir.join("auth.json"),
        read_only: false,
    })
}

struct AuthFile {
    path: PathBuf,
    read_only: bool,
}

impl AuthFile {
    async fn entries(&self, signal: &CancellationToken) -> AiResult<IndexMap<String, Credential>> {
        // The read-only command must not create a lock or even the parent directory.
        let _guard = if !self.read_only && self.path.try_exists()? {
            Some(file_lock(&self.path, signal).await?)
        } else {
            None
        };
        read_entries(&self.path, signal).await
    }
}

#[async_trait]
impl CredentialStore for AuthFile {
    async fn read(
        &self,
        provider: &str,
        signal: &CancellationToken,
    ) -> AiResult<Option<Credential>> {
        let mut entries = self.entries(signal).await?;
        let mut credential = entries.shift_remove(provider);
        if let Some(Credential::ApiKey(api)) = &mut credential
            && let Some(key) = &api.key
            && !(self.read_only && is_command_config_value(key))
        {
            let env = (!api.env.is_empty()).then(|| {
                api.env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            });
            api.key = resolve_config_value_str_with_env(key, provider, "apiKey", env.as_ref());
        }
        Ok(credential)
    }

    async fn list(&self, signal: &CancellationToken) -> AiResult<Vec<CredentialInfo>> {
        let entries = self.entries(signal).await?;
        Ok(entries
            .into_iter()
            .map(|(provider_id, credential)| CredentialInfo {
                provider_id,
                auth_type: credential.auth_type(),
            })
            .collect())
    }

    async fn modify(
        &self,
        provider: &str,
        update: Mutation,
        signal: &CancellationToken,
    ) -> AiResult<Option<Credential>> {
        if self.read_only {
            return Err(Error::Auth(
                "Read-only credential storage cannot modify auth.json".into(),
            ));
        }
        let _guard = file_lock(&self.path, signal).await?;
        let mut entries: IndexMap<String, Credential> = read_entries(&self.path, signal).await?;
        let current = entries.get(provider).cloned();
        let Some(next) = update(current).await? else {
            return Ok(entries.shift_remove(provider));
        };
        entries.insert(provider.to_owned(), next);
        write_entries(&self.path, &entries, signal).await?;
        Ok(entries.shift_remove(provider))
    }

    async fn delete(&self, provider: &str, signal: &CancellationToken) -> AiResult<()> {
        if self.read_only {
            return Err(Error::Auth(
                "Read-only credential storage cannot modify auth.json".into(),
            ));
        }
        let _guard = file_lock(&self.path, signal).await?;
        let mut entries: IndexMap<String, Credential> = read_entries(&self.path, signal).await?;
        entries.shift_remove(provider);
        write_entries(&self.path, &entries, signal).await
    }
}

struct CatalogFile(PathBuf);

#[async_trait]
impl ModelsStore for CatalogFile {
    async fn read(
        &self,
        provider: &str,
        signal: &CancellationToken,
    ) -> AiResult<Option<ModelsStoreEntry>> {
        let mut entries: IndexMap<String, ModelsStoreEntry> = read_entries(&self.0, signal).await?;
        Ok(entries.shift_remove(provider))
    }

    async fn write(
        &self,
        provider: &str,
        entry: ModelsStoreEntry,
        signal: &CancellationToken,
    ) -> AiResult<()> {
        let _guard = file_lock(&self.0, signal).await?;
        let mut entries = read_entries(&self.0, signal).await?;
        entries.insert(provider.to_owned(), entry);
        write_entries(&self.0, &entries, signal).await
    }

    async fn delete(&self, provider: &str, signal: &CancellationToken) -> AiResult<()> {
        let _guard = file_lock(&self.0, signal).await?;
        let mut entries: IndexMap<String, ModelsStoreEntry> = read_entries(&self.0, signal).await?;
        entries.shift_remove(provider);
        write_entries(&self.0, &entries, signal).await
    }
}

fn prepare_file(path: &Path) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(dir) = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty() && !dir.is_dir())
    {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.recursive(true).create(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => file.write_all(b"{}"),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

async fn file_lock(path: &Path, signal: &CancellationToken) -> AiResult<LockGuard> {
    if signal.is_cancelled() {
        return Err(Error::Aborted);
    }
    prepare_file(path)?;
    let options = LockOptions {
        stale_ms: 30_000,
        ..LockOptions::default()
    };
    let mut delay = Duration::from_millis(10);
    loop {
        match lock(path, &options) {
            Ok(guard) => return Ok(guard),
            Err(error) if error == "ELOCKED" => tokio::select! {
                _ = signal.cancelled() => return Err(Error::Aborted),
                _ = tokio::time::sleep(delay) => {
                    delay = (delay * 2).min(Duration::from_secs(1));
                }
            },
            Err(error) => return Err(Error::Auth(error)),
        }
    }
}

async fn read_entries<T: DeserializeOwned>(
    path: &Path,
    signal: &CancellationToken,
) -> AiResult<IndexMap<String, T>> {
    let text = match pi_rs_ai::http::abortable(signal, async {
        Ok(tokio::fs::read_to_string(path).await?)
    })
    .await
    {
        Ok(text) => text,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(IndexMap::new());
        }
        Err(error) => return Err(error),
    };
    Ok(serde_json::from_str(strip_bom(&text))?)
}

async fn write_entries<T: Serialize>(
    path: &Path,
    entries: &IndexMap<String, T>,
    signal: &CancellationToken,
) -> AiResult<()> {
    if signal.is_cancelled() {
        return Err(Error::Aborted);
    }
    let data = serde_json::to_vec_pretty(entries)?;
    tokio::fs::write(path, data).await?;
    Ok(())
}

struct ConfigHeaders {
    provider: String,
    values: IndexMap<String, String>,
    bearer: bool,
}

impl ConfigHeaders {
    fn new(provider: &str, values: Option<IndexMap<String, String>>, bearer: Option<bool>) -> Self {
        Self {
            provider: provider.to_owned(),
            values: values.unwrap_or_default(),
            bearer: bearer.unwrap_or_default(),
        }
    }

    fn apply(&self, auth: &mut ModelAuth, env: &pi_rs_ai::ProviderEnv) -> AiResult<()> {
        if self.values.is_empty() && !self.bearer {
            return Ok(());
        }
        let env =
            (!env.is_empty()).then(|| env.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
        let mut headers = IndexMap::new();
        for (name, value) in &self.values {
            let value =
                resolve_config_value_str_with_env(value, &self.provider, name, env.as_ref())
                    .ok_or_else(|| {
                        Error::Auth(format!(
                            "Unable to resolve header {name} for provider \"{}\"",
                            self.provider
                        ))
                    })?;
            headers.insert(name.clone(), Some(value));
        }
        if self.bearer {
            let key = auth
                .api_key
                .as_deref()
                .filter(|key| !key.is_empty())
                .ok_or_else(|| Error::Auth("authHeader requires a resolved API key".into()))?;
            headers.insert("Authorization".into(), Some(format!("Bearer {key}")));
        }
        auth.headers = Some(pi_rs_ai::merge_headers(
            auth.headers.take().unwrap_or_default(),
            headers,
        ));
        Ok(())
    }
}

struct ConfigAuth {
    origin: Option<Arc<dyn ApiKeyAuth>>,
    key: Option<String>,
    headers: ConfigHeaders,
}

#[async_trait]
impl ApiKeyAuth for ConfigAuth {
    fn name(&self) -> &str {
        self.origin.as_ref().map_or("API key", |auth| auth.name())
    }

    fn interactive(&self) -> bool {
        true
    }

    async fn login(
        &self,
        ui: &dyn AuthInteraction,
        options: &LoginOptions,
    ) -> AiResult<ApiKeyCredential> {
        if let Some(origin) = &self.origin
            && origin.interactive()
        {
            return origin.login(ui, options).await;
        }
        let key = ui
            .prompt(AuthPrompt {
                kind: PromptKind::Secret,
                message: "Enter API key".into(),
                placeholder: None,
                signal: options.signal.clone(),
            })
            .await?;
        Ok(ApiKeyCredential {
            key: Some(key),
            ..ApiKeyCredential::default()
        })
    }

    async fn check(
        &self,
        ctx: &dyn AuthContext,
        key: Option<&ApiKeyCredential>,
        signal: &CancellationToken,
    ) -> AiResult<Option<AuthCheck>> {
        let source = if let Some(stored) = key {
            if let Some(origin) = &self.origin {
                return origin.check(ctx, key, signal).await;
            }
            stored
                .key
                .as_ref()
                .filter(|key| !key.is_empty())
                .map(|_| "stored credential")
        } else if let Some(key) = &self.key {
            for name in get_config_value_env_var_names(key) {
                if ctx.env(&name).await.is_none() {
                    return Ok(None);
                }
            }
            Some("configured API key")
        } else if let Some(origin) = &self.origin {
            return origin.check(ctx, None, signal).await;
        } else {
            None
        };
        Ok(source.map(|source| AuthCheck {
            source: Some(source.into()),
            auth_type: AuthType::ApiKey,
        }))
    }

    async fn resolve(
        &self,
        ctx: &dyn AuthContext,
        key: Option<&ApiKeyCredential>,
        signal: &CancellationToken,
    ) -> AiResult<Option<AuthResult>> {
        let configured;
        let mut source = "stored credential";
        let key = if key.is_none()
            && let Some(raw) = &self.key
        {
            source = "configured API key";
            let mut env = std::collections::HashMap::new();
            for name in get_config_value_env_var_names(raw) {
                if let Some(value) = ctx.env(&name).await {
                    env.insert(name, value);
                }
            }
            let resolved = resolve_config_value_str_with_env(
                raw,
                &self.headers.provider,
                "apiKey",
                Some(&env),
            )
            .ok_or_else(|| {
                Error::Auth(format!(
                    "Unable to resolve API key for provider \"{}\"",
                    self.headers.provider
                ))
            })?;
            configured = ApiKeyCredential {
                key: Some(resolved),
                ..ApiKeyCredential::default()
            };
            Some(&configured)
        } else {
            key
        };
        let mut result = if let Some(origin) = &self.origin {
            origin.resolve(ctx, key, signal).await?
        } else {
            key.and_then(|key| {
                key.key
                    .as_ref()
                    .filter(|key| !key.is_empty())
                    .map(|value| AuthResult {
                        auth: ModelAuth {
                            api_key: Some(value.clone()),
                            ..ModelAuth::default()
                        },
                        source: Some(source.into()),
                        env: key.env.clone(),
                    })
            })
        };
        if let Some(result) = &mut result {
            self.headers.apply(&mut result.auth, &result.env)?;
        }
        Ok(result)
    }
}

struct ConfigOAuth {
    origin: Arc<dyn OAuthAuth>,
    headers: ConfigHeaders,
}

#[async_trait]
impl OAuthAuth for ConfigOAuth {
    fn name(&self) -> &str {
        self.origin.name()
    }
    fn is_subscription(&self) -> bool {
        self.origin.is_subscription()
    }
    fn login_label(&self) -> Option<&str> {
        self.origin.login_label()
    }

    async fn login(
        &self,
        ui: &dyn AuthInteraction,
        options: &LoginOptions,
    ) -> AiResult<OAuthCredential> {
        self.origin.login(ui, options).await
    }

    async fn refresh(
        &self,
        credential: OAuthCredential,
        signal: CancellationToken,
    ) -> AiResult<OAuthCredential> {
        self.origin.refresh(credential, signal).await
    }

    async fn to_auth(&self, credential: &OAuthCredential) -> AiResult<ModelAuth> {
        let mut auth = self.origin.to_auth(credential).await?;
        let env = credential
            .details
            .get("env")
            .and_then(|value| value.as_object())
            .into_iter()
            .flatten()
            .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_owned())))
            .collect();
        self.headers.apply(&mut auth, &env)?;
        Ok(auth)
    }
}
