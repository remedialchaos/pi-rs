#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, mpsc};

use async_trait::async_trait;
use pi_rs_ai::auth::{
    AuthEvent, AuthInteraction, AuthPrompt, Credential, CredentialStore, FileCredentialStore,
    LoginOptions, OAuthAuth, PromptKind,
};
use pi_rs_ai::oauth::BuiltinOAuth;
use pi_rs_ai::providers::builtin_provider_specs;
use pi_rs_ai::{Error, Result};
use tokio::sync::oneshot;

const AUTH_FILE: &str = "auth.json";
struct Terminal(mpsc::Sender<oneshot::Sender<std::io::Result<String>>>);
impl Terminal {
    fn new() -> Result<Self> {
        let (prompts, requests) = mpsc::channel::<oneshot::Sender<std::io::Result<String>>>();
        // A loopback callback may win while stdin is still waiting. Do not put the read on the runtime's blocking pool.
        std::thread::Builder::new()
            .name("pi-ai-stdin".into())
            .spawn(move || {
                let stdin = std::io::stdin();
                while let Ok(answer) = requests.recv() {
                    if answer.is_closed() {
                        continue;
                    }
                    let mut line = String::new();
                    let result = stdin.read_line(&mut line).map(|_| line);
                    let _ = answer.send(result);
                }
            })?;
        Ok(Self(prompts))
    }
    async fn line(&self, prompt: &str) -> Result<String> {
        print!("{prompt}");
        std::io::stdout().flush()?;
        let (answer, line) = oneshot::channel();
        self.0
            .send(answer)
            .map_err(|_| Error::Protocol("Terminal input is closed".into()))?;
        let mut line = line
            .await
            .map_err(|_| Error::Protocol("Terminal input is closed".into()))??;
        let trimmed = line.trim_end_matches(['\r', '\n']).len();
        line.truncate(trimmed);
        Ok(line)
    }
    async fn selection(&self, count: usize) -> Result<Option<usize>> {
        let line = self.line(&format!("Enter number (1-{count}): ")).await?;
        let input = line
            .trim_start()
            .strip_prefix('+')
            .unwrap_or_else(|| line.trim_start());
        let end = input.bytes().take_while(u8::is_ascii_digit).count();
        Ok(input[..end]
            .parse::<usize>()
            .ok()
            .and_then(|i| i.checked_sub(1))
            .filter(|i| *i < count))
    }
}
#[async_trait]
impl AuthInteraction for Terminal {
    async fn prompt(&self, prompt: AuthPrompt) -> Result<String> {
        if let PromptKind::Select(items) = prompt.kind {
            println!("\n{}", prompt.message);
            for (i, item) in items.iter().enumerate() {
                println!("  {}. {}", i + 1, item.label);
            }
            let choice = self
                .selection(items.len())
                .await?
                .ok_or_else(|| Error::Auth("Invalid selection".into()))?;
            return Ok(items[choice].id.clone());
        }
        let placeholder = prompt
            .placeholder
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        self.line(&format!("{}{placeholder}: ", prompt.message))
            .await
    }
    fn notify(&self, event: AuthEvent) {
        match event {
            AuthEvent::AuthUrl { url, instructions } => {
                println!("\nOpen this URL in your browser:\n{url}");
                if let Some(instructions) = instructions {
                    println!("{instructions}");
                }
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                ..
            } => println!(
                "\nOpen this URL in your browser:\n{verification_uri}\nEnter code: {user_code}"
            ),
            AuthEvent::Progress(message) | AuthEvent::Info { message, .. } => println!("{message}"),
        }
    }
}
async fn run() -> Result<()> {
    let mut argv = std::env::args().skip(1);
    let providers = builtin_provider_specs()?
        .iter()
        .filter_map(|p| {
            p.oauth_flow()
                .map(|flow| (p.id.as_str(), p.name.as_str(), flow))
        })
        .collect::<Vec<_>>();
    match argv.next().as_deref() {
        None | Some("help" | "--help" | "-h") => {
            println!(
                "Usage: npx @earendil-works/pi-ai <command> [provider]\n\nCommands:\n  login [provider]  Login to an OAuth provider\n  list              List available providers\n\nProviders:"
            );
            for (id, name, _) in &providers {
                println!("  {id:20} {name}");
            }
        }
        Some("list") => {
            for (id, name, _) in &providers {
                println!("{id:20} {name}");
            }
        }
        Some("login") => {
            let terminal = Terminal::new()?;
            let id = if let Some(id) = argv.next().filter(|id| !id.is_empty()) {
                id
            } else {
                for (i, (_, name, _)) in providers.iter().enumerate() {
                    println!("  {}. {name}", i + 1);
                }
                terminal
                    .selection(providers.len())
                    .await?
                    .map(|i| providers[i].0.to_owned())
                    .unwrap_or_default()
            };
            let (_, _, flow) = providers
                .iter()
                .find(|(key, _, _)| *key == id)
                .ok_or_else(|| Error::Provider(id.clone()))?;
            let opts = LoginOptions {
                get_device_id: Some(Arc::new(|| uuid::Uuid::new_v4().to_string())),
                ..LoginOptions::default()
            };
            let credential = BuiltinOAuth::new(*flow)?.login(&terminal, &opts).await?;
            let store = FileCredentialStore::new(PathBuf::from(AUTH_FILE));
            store
                .modify(
                    &id,
                    Box::new(|_| Box::pin(async move { Ok(Some(Credential::OAuth(credential))) })),
                    &opts.signal,
                )
                .await?;
            println!("\nCredentials saved to {AUTH_FILE}");
        }
        Some(command) => return Err(Error::Protocol(format!("Unknown command: {command}"))),
    }
    Ok(())
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}
