//! `pi auth login`: drive the provider's native OAuth or API-key interaction.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pi_rs_agent::CancellationToken;
use pi_rs_ai::auth::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthType, LoginOptions, PromptKind, SelectOption,
};
use pi_rs_ai::registry::RefreshOptions;
use pi_rs_ai::{Error, Result as AiResult};
use pi_rs_mcp::{
    McpServerConfigOptions, McpServerConfigResult, mcp_namespace, validate_mcp_server_config,
};
use serde_json::{Value, json};

use super::auth_command::{Runtime, find_provider, js_trim};
use super::auth_terminal::{LoginTerminal, ProcessLoginTerminal};
use crate::core::settings_manager::{CreateOptions, SettingsManager};
use crate::utils::text::strip_bom;
use crate::utils::web_url::{display_url, replace_control_characters, web_url};

const USAGE: &str = "pi auth login <provider> [--api-key] [--no-browser]";
const RADIUS_MCP_URL: &str = "https://radius.pi.dev/mcp";

struct Request<'a> {
    provider: &'a str,
    api_key: bool,
    browser: bool,
}

fn parse_request<'a>(args: &[&'a str]) -> Result<Request<'a>, String> {
    let mut request = Request {
        provider: "",
        api_key: false,
        browser: true,
    };
    for arg in args {
        match *arg {
            "--api-key" => request.api_key = true,
            "--no-browser" => request.browser = false,
            _ if arg.starts_with('-') || !request.provider.is_empty() => {
                return Err(format!("Usage: {USAGE}"));
            }
            _ => request.provider = arg,
        }
    }
    if request.provider.is_empty() {
        return Err(format!("Usage: {USAGE}"));
    }
    if request.api_key && !request.browser {
        return Err("--no-browser only applies to account sign-in; omit it with --api-key.".into());
    }
    Ok(request)
}

pub(super) async fn run(args: &[&str], cwd: &Path, agent_dir: &Path, offline: bool) -> u8 {
    let signal = CancellationToken::new();
    let terminal = ProcessLoginTerminal::new(signal.clone());
    let interrupt = signal.clone();
    let listener = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            interrupt.cancel();
        }
    });
    let code = run_with_terminal(args, cwd, agent_dir, offline, signal, &terminal).await;
    listener.abort();
    code
}

pub(crate) async fn run_with_terminal(
    args: &[&str],
    cwd: &Path,
    agent_dir: &Path,
    offline: bool,
    signal: CancellationToken,
    terminal: &dyn LoginTerminal,
) -> u8 {
    let request = match parse_request(args) {
        Ok(request) => request,
        Err(error) => {
            terminal.error(&error);
            return 1;
        }
    };
    let settings = SettingsManager::create_with_dirs(
        cwd,
        agent_dir,
        CreateOptions {
            project_trusted: false,
        },
    );
    if let Err(error) = settings.get_http_idle_timeout_ms() {
        terminal.error(&error);
        return 1;
    }
    let runtime = match Runtime::new(agent_dir, false, false) {
        Ok(runtime) => runtime,
        Err(error) => {
            terminal.error(&error);
            return 1;
        }
    };
    let options = LoginOptions {
        get_device_id: Some(device_id(settings)),
        signal,
        ..LoginOptions::default()
    };
    login(request, &runtime, terminal, agent_dir, &options, offline).await
}

fn device_id(settings: SettingsManager) -> Arc<dyn Fn() -> String + Send + Sync> {
    let device_id = std::sync::LazyLock::new(move || {
        let mut settings = settings;
        if let Some(id) = settings.get_global_settings()["deviceId"]
            .as_str()
            .filter(|id| !id.is_empty())
        {
            return id.to_owned();
        }
        let id = uuid::Uuid::new_v4().to_string();
        let _ = settings.set_value(&["deviceId".into()], Value::String(id.clone()));
        id
    });
    Arc::new(move || (*device_id).clone())
}

async fn login(
    request: Request<'_>,
    runtime: &Runtime,
    terminal: &dyn LoginTerminal,
    agent_dir: &Path,
    options: &LoginOptions,
    offline: bool,
) -> u8 {
    let providers = runtime.models().get_providers();
    let Some(provider) = find_provider(&providers, request.provider) else {
        let mut login_providers: Vec<_> = providers
            .iter()
            .filter(|provider| {
                provider.auth.oauth.is_some()
                    || provider
                        .auth
                        .api_key
                        .as_ref()
                        .is_some_and(|auth| auth.interactive())
            })
            .map(|provider| provider.id.as_str())
            .collect();
        login_providers.sort_unstable();
        terminal.error(&format!(
            "Unknown provider \"{}\".",
            replace_control_characters(request.provider)
        ));
        terminal.error(&format!("Providers: {}", login_providers.join(", ")));
        return 1;
    };
    if !terminal.interactive() {
        terminal.error(&format!("\"{USAGE}\" requires an interactive terminal"));
        return 1;
    }
    let auth_type = if !request.api_key && provider.auth.oauth.is_some() {
        AuthType::OAuth
    } else if provider
        .auth
        .api_key
        .as_ref()
        .is_some_and(|auth| auth.interactive())
    {
        AuthType::ApiKey
    } else {
        if request.api_key && provider.auth.oauth.is_some() {
            terminal.error(&format!(
                "{} has no API key login. Omit --api-key to sign in.",
                provider.name
            ));
        } else {
            let method = provider
                .auth
                .api_key
                .as_ref()
                .map_or("ambient credentials", |auth| auth.name());
            terminal.error(&format!(
                "{} is configured outside pi ({method}).",
                provider.name
            ));
        }
        return 1;
    };
    let interaction = Interaction {
        terminal,
        open_browser: request.browser,
        method: if auth_type == AuthType::ApiKey {
            Method::Ask
        } else if request.browser {
            Method::Browser
        } else {
            Method::Other
        },
        provider: &provider.id,
        skipped: Mutex::default(),
        signal: &options.signal,
    };
    let saved = runtime
        .models()
        .login(&provider.id, auth_type, &interaction, options)
        .await;
    terminal.settle().await;
    if let Err(error) = saved {
        if options.signal.is_cancelled()
            || matches!(error, Error::Aborted)
            || error.to_string() == "Login cancelled"
        {
            terminal.error("Login cancelled.");
        } else {
            terminal.error(&format!("Failed to log in to {}: {error}", provider.name));
        }
        return 1;
    }
    if let Err(error) = runtime.synchronize(&provider.id, &options.signal).await {
        terminal.error(&format!(
            "Logged in to {}, but local model state could not be synchronized: {error}",
            provider.name
        ));
        return 1;
    }
    let action = if auth_type == AuthType::OAuth {
        "Logged in to"
    } else {
        "Saved API key for"
    };
    terminal.write(&format!(
        "{action} {}. Credentials saved to {}",
        provider.name,
        agent_dir.join("auth.json").display()
    ));
    let refresh_signal = options.signal.child_token();
    let refresh = runtime.models().refresh(RefreshOptions {
        providers: Some(vec![provider.id.clone()]),
        allow_network: !offline,
        signal: refresh_signal.clone(),
    });
    let refreshed = tokio::time::timeout(Duration::from_secs(15), refresh).await;
    refresh_signal.cancel();
    if options.signal.is_cancelled() {
        terminal.write(&format!(
            "Skipped the {} model catalog refresh; pi refreshes it on next start.",
            provider.name
        ));
    } else {
        match refreshed {
            Err(_) => terminal.write(&format!(
                "The {} model catalog refresh timed out; pi refreshes it on next start.",
                provider.name
            )),
            Ok(result) if !result.errors.is_empty() => terminal.write(&format!(
                "The {} model catalog could not be refreshed; pi retries on next start.",
                provider.name
            )),
            Ok(_) => {}
        }
    }
    if provider.id == "radius" {
        let path = agent_dir.join("mcp.json");
        match configure_radius_mcp(&path, &provider.id) {
            Ok(Some((name, replaces))) => {
                let action = if replaces { "Updated" } else { "Added" };
                terminal.write(&format!(
                    "{action} MCP server \"{name}\" in {} to use the {} login.",
                    path.display(),
                    provider.name
                ));
            }
            Ok(None) => {}
            Err(error) => terminal.error(&format!("Could not update {}: {error}", path.display())),
        }
    }
    0
}

#[derive(Clone, Copy)]
enum Method {
    Ask,
    Browser,
    Other,
}

struct Interaction<'a> {
    terminal: &'a dyn LoginTerminal,
    open_browser: bool,
    method: Method,
    provider: &'a str,
    skipped: Mutex<Vec<String>>,
    signal: &'a CancellationToken,
}

impl Interaction<'_> {
    async fn select(
        &self,
        message: &str,
        options: &[SelectOption],
        signal: &CancellationToken,
    ) -> AiResult<String> {
        if let Some((first, others)) = options.split_first() {
            match self.method {
                Method::Browser => {
                    *self
                        .skipped
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) =
                        others.iter().map(|option| option.label.clone()).collect();
                    return Ok(first.id.clone());
                }
                Method::Other => match others {
                    [] => return Ok(first.id.clone()),
                    [only] => {
                        self.terminal.write(&format!("{message} {}", only.label));
                        return Ok(only.id.clone());
                    }
                    _ => return self.select_in_terminal(message, others, signal).await,
                },
                Method::Ask => {}
            }
        }
        self.select_in_terminal(message, options, signal).await
    }

    async fn select_in_terminal(
        &self,
        message: &str,
        options: &[SelectOption],
        signal: &CancellationToken,
    ) -> AiResult<String> {
        self.terminal.write(message);
        for (index, option) in options.iter().enumerate() {
            let number = index + 1;
            if let Some(description) = option
                .description
                .as_deref()
                .filter(|text| !text.is_empty())
            {
                self.terminal
                    .write(&format!("  {number}. {} - {description}", option.label));
            } else {
                self.terminal
                    .write(&format!("  {number}. {}", option.label));
            }
        }
        let count = options.len();
        let prompt = format!("Select 1-{count}: ");
        loop {
            let answer = self.terminal.prompt(&prompt, false, signal).await?;
            let answer = js_trim(&answer);
            if answer.bytes().all(|byte| byte.is_ascii_digit())
                && let Some(index) = answer
                    .parse::<usize>()
                    .ok()
                    .and_then(|index| index.checked_sub(1))
                && let Some(option) = options.get(index)
            {
                return Ok(option.id.clone());
            }
            self.terminal
                .write(&format!("Enter a number from 1 to {count}."));
        }
    }
}

#[async_trait]
impl AuthInteraction for Interaction<'_> {
    async fn prompt(&self, prompt: AuthPrompt) -> AiResult<String> {
        let signal = &prompt.signal;
        if signal.is_cancelled() || self.signal.is_cancelled() {
            return Err(Error::Aborted);
        }
        let answer = async {
            if let PromptKind::Select(options) = &prompt.kind {
                return self.select(&prompt.message, options, signal).await;
            }
            let message = match prompt
                .placeholder
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                Some(placeholder) => format!("{} ({placeholder}): ", prompt.message),
                None => format!("{}: ", prompt.message),
            };
            self.terminal
                .prompt(&message, prompt.kind == PromptKind::Secret, signal)
                .await
        };
        tokio::select! {
            biased;
            _ = self.signal.cancelled() => Err(Error::Aborted),
            _ = signal.cancelled() => Err(Error::Aborted),
            answer = answer => answer,
        }
    }

    fn notify(&self, event: AuthEvent) {
        match event {
            AuthEvent::AuthUrl { url, instructions } => {
                self.terminal.write(&format!(
                    "Open this URL to sign in:\n  {}",
                    display_url(&url)
                ));
                if let Some(instructions) = instructions.filter(|value| !value.is_empty()) {
                    self.terminal.write(&instructions);
                }
                if self.open_browser
                    && let Some(target) = web_url(&url)
                {
                    self.terminal.open_url(&target);
                }
                let skipped = self
                    .skipped
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                match skipped.as_slice() {
                    [] => {}
                    [only] => self.terminal.write(&format!(
                        "To use \"{only}\" instead, run \"pi auth login {} --no-browser\"",
                        self.provider
                    )),
                    others => {
                        let labels = others
                            .iter()
                            .map(|label| format!("\"{label}\""))
                            .collect::<Vec<_>>()
                            .join(", ");
                        self.terminal.write(&format!("Other sign-in methods: {labels}. Run \"pi auth login {} --no-browser\" to choose one.", self.provider));
                    }
                }
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                open_browser_on_confirm,
                ..
            } => {
                self.terminal.write(&format!(
                    "Open {} and enter code: {user_code}",
                    display_url(&verification_uri)
                ));
                if self.open_browser
                    && open_browser_on_confirm
                    && let Some(target) = web_url(&verification_uri)
                {
                    self.terminal.open_url(&target);
                }
                self.terminal.write("Waiting for authentication...");
            }
            AuthEvent::Info { message, links } => {
                self.terminal.write(&message);
                for (url, label) in links {
                    let url = display_url(&url);
                    if let Some(label) = label.filter(|value| !value.is_empty()) {
                        self.terminal.write(&format!("  {label}: {url}"));
                    } else {
                        self.terminal.write(&format!("  {url}"));
                    }
                }
            }
            AuthEvent::Progress(message) => self.terminal.write(&message),
        }
    }
}

fn configure_radius_mcp(path: &Path, provider: &str) -> Result<Option<(String, bool)>, String> {
    let (text, mut raw): (_, Value) = match std::fs::read_to_string(path) {
        Ok(text) => {
            let parsed =
                serde_json::from_str(strip_bom(&text)).map_err(|error| error.to_string())?;
            (text, parsed)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (String::new(), json!({})),
        Err(error) => return Err(error.to_string()),
    };
    let invalid = || {
        format!(
            "{}: expected an object with an \"mcpServers\" object",
            path.display()
        )
    };
    let servers = raw
        .as_object_mut()
        .ok_or_else(invalid)?
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(invalid)?;
    let mut namespaces = HashSet::new();
    let options = McpServerConfigOptions::default();
    let mut existing = None;
    for (name, config) in servers.iter() {
        let validated = validate_mcp_server_config(name, config, &options);
        if matches!(validated, McpServerConfigResult::Invalid(_))
            || !namespaces.insert(mcp_namespace(name))
        {
            continue;
        }
        if let McpServerConfigResult::Http(config) = validated
            && config["url"]
                .as_str()
                .is_some_and(|url| url.trim_end_matches('/') == RADIUS_MCP_URL)
        {
            if config["auth"]["provider"].as_str() == Some(provider) {
                return Ok(None);
            }
            existing = Some((name.clone(), config));
            break;
        }
    }
    let (name, mut config) = existing.unwrap_or_else(|| {
        let mut name = "radius".to_owned();
        for index in 1.. {
            if !namespaces.contains(&mcp_namespace(&name)) {
                break;
            }
            name = if index == 1 {
                "radius-mcp".into()
            } else {
                format!("radius-mcp-{index}")
            };
        }
        (name, json!({"url": RADIUS_MCP_URL}))
    });
    let config = config.as_object_mut().ok_or_else(invalid)?;
    config.remove("oauth");
    config.insert("auth".into(), json!({"provider": provider}));
    let replaces = servers
        .insert(name.clone(), Value::Object(std::mem::take(config)))
        .is_some();
    let indent = text
        .lines()
        .find_map(|line| {
            let trimmed = line.trim_start_matches([' ', '\t']);
            let width = line.len() - trimmed.len();
            (width > 0 && !trimmed.trim().is_empty()).then(|| &line.as_bytes()[..width])
        })
        .unwrap_or(b"  ");
    let mut output = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(
        &mut output,
        serde_json::ser::PrettyFormatter::with_indent(indent),
    );
    serde::Serialize::serialize(&raw, &mut serializer).map_err(|error| error.to_string())?;
    output.push(b'\n');
    std::fs::write(path, output).map_err(|error| error.to_string())?;
    Ok(Some((name, replaces)))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use pi_rs_ai::auth::Credential;

    use super::*;

    #[derive(Default)]
    struct ScriptedTerminal {
        answers: Mutex<VecDeque<String>>,
        prompts: Mutex<Vec<(String, bool)>>,
        lines: Mutex<Vec<String>>,
        urls: Mutex<Vec<String>>,
        errors: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl LoginTerminal for ScriptedTerminal {
        fn interactive(&self) -> bool {
            true
        }

        fn write(&self, line: &str) {
            self.lines
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(line.into());
        }

        fn error(&self, line: &str) {
            self.errors
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(line.into());
        }

        fn open_url(&self, url: &str) {
            self.urls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(url.into());
        }

        async fn prompt(
            &self,
            message: &str,
            secret: bool,
            signal: &CancellationToken,
        ) -> AiResult<String> {
            if signal.is_cancelled() {
                return Err(Error::Aborted);
            }
            self.prompts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((message.into(), secret));
            self.answers
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pop_front()
                .ok_or_else(|| Error::Auth(format!("Unexpected prompt: {message}")))
        }

        async fn settle(&self) {}
    }

    #[tokio::test]
    async fn login_saves_a_real_provider_api_key_without_echoing_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let runtime = Runtime::new(dir.path(), false, false)?;
        let terminal = ScriptedTerminal {
            answers: Mutex::new(VecDeque::from(["test-key".into()])),
            ..ScriptedTerminal::default()
        };
        let options = LoginOptions::default();
        let request = parse_request(&["  OpenAI ", "--api-key"])?;
        assert_eq!(
            login(request, &runtime, &terminal, dir.path(), &options, true).await,
            0
        );
        assert_eq!(
            *terminal.prompts.lock().map_err(|e| e.to_string())?,
            [("Enter OpenAI API key: ".into(), true)]
        );
        let current = runtime.credentials.read("openai", &options.signal).await?;
        assert_eq!(
            match current {
                Some(Credential::ApiKey(credential)) => credential.key,
                _ => None,
            },
            Some("test-key".into())
        );
        let lines = terminal.lines.lock().map_err(|e| e.to_string())?;
        assert_eq!(
            *lines,
            [format!(
                "Saved API key for OpenAI. Credentials saved to {}",
                dir.path().join("auth.json").display()
            )]
        );
        assert!(
            terminal
                .errors
                .lock()
                .map_err(|e| e.to_string())?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn no_browser_adapts_sign_in_choices_and_prompt_cancellation()
    -> Result<(), Box<dyn std::error::Error>> {
        let signal = CancellationToken::new();
        let terminal = ScriptedTerminal {
            answers: Mutex::new(VecDeque::from(["7".into(), " 2 ".into()])),
            ..ScriptedTerminal::default()
        };
        let interaction = Interaction {
            terminal: &terminal,
            open_browser: false,
            method: Method::Other,
            provider: "radius",
            skipped: Mutex::default(),
            signal: &signal,
        };
        let options = [
            ("browser", "Browser"),
            ("device", "Device"),
            ("copy", "Copy code"),
        ]
        .map(|(id, label)| SelectOption {
            id: id.into(),
            label: label.into(),
            description: None,
        });
        let prompt = AuthPrompt {
            kind: PromptKind::Select(options.into()),
            message: "Sign in:".into(),
            placeholder: None,
            signal: signal.child_token(),
        };
        assert_eq!(interaction.prompt(prompt.clone()).await?, "copy");
        prompt.signal.cancel();
        assert!(matches!(
            interaction.prompt(prompt).await,
            Err(Error::Aborted)
        ));
        assert!(!signal.is_cancelled());
        interaction.notify(AuthEvent::DeviceCode {
            user_code: "ABCD".into(),
            verification_uri: "https://example.test/device".into(),
            interval_seconds: 5,
            expires_in_seconds: None,
            open_browser_on_confirm: true,
            copy_code_on_confirm: false,
        });
        assert!(terminal.urls.lock().map_err(|e| e.to_string())?.is_empty());
        assert_eq!(
            *terminal.lines.lock().map_err(|e| e.to_string())?,
            [
                "Sign in:",
                "  1. Device",
                "  2. Copy code",
                "Enter a number from 1 to 2.",
                "Open https://example.test/device and enter code: ABCD",
                "Waiting for authentication...",
            ]
        );
        Ok(())
    }

    #[test]
    fn radius_mcp_setup_preserves_other_names_and_is_idempotent()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("mcp.json");
        let initial =
            json!({"mcpServers": {"radius":{"command":"one"}, "radius_mcp":{"command":"two"}}});
        std::fs::write(&path, serde_json::to_vec_pretty(&initial)?)?;
        assert_eq!(
            configure_radius_mcp(&path, "radius")?,
            Some(("radius-mcp-2".into(), false))
        );
        let parsed: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert_eq!(
            parsed["mcpServers"]["radius_mcp"],
            initial["mcpServers"]["radius_mcp"]
        );
        assert_eq!(
            parsed["mcpServers"]["radius-mcp-2"],
            json!({"url": RADIUS_MCP_URL,"auth":{"provider":"radius"}})
        );
        assert_eq!(configure_radius_mcp(&path, "radius")?, None);
        Ok(())
    }
}
