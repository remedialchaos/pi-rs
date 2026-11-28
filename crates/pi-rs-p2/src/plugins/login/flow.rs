use std::{
    collections::HashMap,
    fs::OpenOptions,
    future::Future,
    io::{self, ErrorKind},
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use pi_rs_ai::{
    Error as AiError,
    auth::{
        AuthEvent, AuthInteraction, AuthPrompt, AuthType, LoginOptions, PromptKind, SelectOption,
    },
};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    host::{Presence, WeakHost},
    state::Source,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};
use uuid::Uuid;

use crate::{
    api::v1::{Principal, server::service_of},
    runtime::logs::process_logs,
};

use super::{LoginResources, MODEL_RESOURCES};

/// How long an unfinished login survives its owner having no page that holds it.
pub(super) const AWAY_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy)]
enum Ending {
    Cancelled,
    Away,
    Replaced,
}

pub(super) struct Logins {
    state: Source,
    inner: Mutex<Inner>,
    host: WeakHost,
    lifetime: LoginOptions,
    away_timeout: Duration,
    credentials_operation: tokio::sync::Mutex<()>,
}

struct Inner {
    active: Option<Attempt>,
    owner: Option<String>,
    login: Value,
}

struct Attempt {
    id: String,
    owner: String,
    remote: bool,
    options: LoginOptions,
    done: watch::Receiver<bool>,
    prompts: HashMap<String, PendingPrompt>,
    ending: Option<Ending>,
    away_timer: Option<tokio::task::JoinHandle<()>>,
}

struct PendingPrompt {
    options: Option<Vec<SelectOption>>,
    sensitive: bool,
    answer: oneshot::Sender<String>,
}

struct Interaction {
    logins: Arc<Logins>,
    resources: Arc<LoginResources>,
    id: String,
    options: LoginOptions,
    deadline_expired: Arc<AtomicBool>,
}

impl Logins {
    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub(super) fn with_away(host: WeakHost, state: Source, away_timeout: Duration) -> Self {
        Self {
            host,
            state,
            inner: Mutex::new(Inner {
                active: None,
                owner: None,
                login: Value::Null,
            }),
            lifetime: LoginOptions::default(),
            away_timeout,
            credentials_operation: tokio::sync::Mutex::new(()),
        }
    }

    pub(super) async fn call(self: Arc<Self>, method: &str, args: Value) -> Result<Value> {
        match method {
            "start" => {
                let _credentials = self.credentials_operation.lock().await;
                let owner = text(&args, "owner")?;
                let provider = text(&args, "providerId")?;
                let kind = serde_json::from_value(args["authType"].clone())?;
                let id = self
                    .start(owner, provider, kind, args["remote"] == true)
                    .await?;
                Ok(json!({"attemptId":id}))
            }
            "answer" => {
                let mut inner = self.lock_inner();
                let attempt = owned(&mut inner, &args)?;
                let prompt_id = text(&args, "promptId")?;
                let value = text(&args, "value")?;
                let pending = attempt
                    .prompts
                    .get(prompt_id)
                    .ok_or_else(|| invalid("Login prompt is no longer active"))?;
                if pending
                    .options
                    .as_ref()
                    .is_some_and(|items| !items.iter().any(|option| option.id == value))
                {
                    return Err(invalid("Select one of the listed options"));
                }
                if pending.sensitive {
                    process_logs().add_secret(value);
                }
                let prompt = attempt.prompts.remove(prompt_id).ok_or(Error::Closed)?;
                if inner.login["prompt"]["id"] == prompt_id {
                    inner.login["prompt"] = Value::Null;
                    self.publish(&inner)?;
                }
                prompt
                    .answer
                    .send(value.to_owned())
                    .map_err(|_| invalid("Login prompt is no longer active"))?;
                Ok(Value::Null)
            }
            "cancel" => {
                let done = {
                    let mut inner = self.lock_inner();
                    let attempt = owned(&mut inner, &args)?;
                    attempt.ending.get_or_insert(Ending::Cancelled);
                    attempt.options.signal.cancel();
                    attempt.done.clone()
                };
                wait(done).await;
                Ok(Value::Null)
            }
            "dismiss" => {
                let mut inner = self.lock_inner();
                if inner.owner.as_deref() == Some(text(&args, "owner")?)
                    && inner.login["attemptId"] == text(&args, "attemptId")?
                    && inner.login["status"] != "working"
                {
                    inner.login = Value::Null;
                    self.publish(&inner)?;
                }
                Ok(Value::Null)
            }
            "logout" => {
                let _credentials = self.credentials_operation.lock().await;
                if self.lock_inner().active.is_some() {
                    return Err(invalid(
                        "Cancel the active login before removing credentials",
                    ));
                }
                let provider = text(&args, "providerId")?;
                let signal = &self.lifetime.signal;
                let resources = self.resources()?;
                resources
                    .models
                    .logout(provider, signal)
                    .await
                    .map_err(|_| invalid("Could not remove provider credentials"))?;
                resources
                    .model_store
                    .delete(provider, signal)
                    .await
                    .map_err(|_| invalid("Could not remove the provider model cache"))?;
                if let Some(registered) = resources.models.get_provider(provider) {
                    resources.models.set_provider(registered);
                }
                (resources.publish)().await?;
                Ok(Value::Null)
            }
            "refresh" => Ok(
                json!({"models":self.resources()?.refresh(text(&args, "providerId")?, &self.lifetime).await?}),
            ),
            _ => Err(invalid("Unknown login method")),
        }
    }

    fn resources(&self) -> Result<Arc<LoginResources>> {
        service_of(&self.host.upgrade().ok_or(Error::Closed)?, MODEL_RESOURCES)
    }

    async fn start(
        self: &Arc<Self>,
        owner: &str,
        provider: &str,
        kind: AuthType,
        remote: bool,
    ) -> Result<String> {
        if self.lifetime.signal.is_cancelled() {
            return Err(invalid("Server is shutting down"));
        }
        let resources = self.resources()?;
        let supported =
            resources
                .models
                .get_provider(provider)
                .is_some_and(|provider| match kind {
                    AuthType::OAuth => provider.auth.oauth.is_some(),
                    AuthType::ApiKey => provider
                        .auth
                        .api_key
                        .as_ref()
                        .is_some_and(|key| key.interactive()),
                });
        if !supported {
            return Err(invalid("Provider does not support this login method"));
        }
        let replaced_done = {
            let mut inner = self.lock_inner();
            match &mut inner.active {
                Some(active) if active.owner == owner => {
                    active.ending.get_or_insert(Ending::Replaced);
                    active.options.signal.cancel();
                    Some(active.done.clone())
                }
                Some(_) => {
                    return Err(invalid(
                        "Another owner device is signing in to a provider; try again when it has finished",
                    ));
                }
                None => None,
            }
        };
        if let Some(done) = replaced_done {
            wait(done).await;
        }
        let mut inner = self.lock_inner();
        if self.lifetime.signal.is_cancelled() {
            return Err(invalid("Server is shutting down"));
        }
        if let Some(active) = &inner.active {
            return Err(invalid(if active.owner == owner {
                "A provider login is already in progress; cancel it first"
            } else {
                "Another owner device is signing in to a provider; try again when it has finished"
            }));
        }
        let id = Uuid::new_v4().to_string();
        let (finished, done) = watch::channel(false);
        let options = LoginOptions {
            signal: self.lifetime.signal.child_token(),
            ..Default::default()
        };
        inner.active = Some(Attempt {
            id: id.clone(),
            owner: owner.into(),
            remote,
            options: options.clone(),
            done,
            prompts: HashMap::new(),
            ending: None,
            away_timer: None,
        });
        inner.owner = Some(owner.to_owned());
        inner.login = json!({
            "attemptId":id,"providerId":provider,"authType":kind,"status":"working","prompt":null,"notices":[],
        });
        if let Err(error) = self.publish(&inner) {
            inner.active.take();
            return Err(error);
        }
        let deadline_expired = Arc::new(AtomicBool::new(false));
        let interaction = Interaction {
            logins: self.clone(),
            resources,
            id: id.clone(),
            options: options.clone(),
            deadline_expired: deadline_expired.clone(),
        };
        let provider = provider.to_owned();
        tokio::spawn(async move {
            let timeout = options.signal.clone();
            let expired = deadline_expired.clone();
            let deadline = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(600)).await;
                expired.store(true, Ordering::Release);
                timeout.cancel();
            });
            let outcome = interaction.run(&provider, kind).await;
            deadline.abort();
            interaction.finish(outcome);
            options.signal.cancel();
            finished.send_replace(true);
        });
        Ok(id)
    }

    pub(super) fn sync_presence(self: &Arc<Self>, references: &[Presence]) {
        let mut inner = self.lock_inner();
        let Some(active) = inner.active.as_mut() else {
            return;
        };
        let owner_present = references.iter().any(|entry| {
            entry
                .principal
                .as_ref()
                .and_then(|principal| Principal::deserialize(principal).ok())
                .is_some_and(|principal| principal.key() == active.owner)
        });
        if owner_present {
            if let Some(timer) = active.away_timer.take() {
                timer.abort();
            }
        } else if active.away_timer.is_none() {
            let logins = self.clone();
            let attempt_id = active.id.clone();
            let delay = self.away_timeout;
            active.away_timer = Some(tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let mut inner = logins.lock_inner();
                if let Some(active) = inner.active.as_mut()
                    && active.id == attempt_id
                {
                    active.ending.get_or_insert(Ending::Away);
                    active.options.signal.cancel();
                }
            }));
        }
    }

    pub(super) async fn close(&self) {
        self.lifetime.signal.cancel();
        let done = self
            .lock_inner()
            .active
            .as_ref()
            .map(|attempt| attempt.done.clone());
        if let Some(done) = done {
            wait(done).await;
        }
    }

    fn publish(&self, inner: &Inner) -> Result<()> {
        self.state.set(
            json!({"login":inner.login,"owner":inner.owner}).into(),
            &Context::background(),
        )
    }
}

impl Interaction {
    async fn run(&self, provider: &str, kind: AuthType) -> (&'static str, Option<&'static str>) {
        let login = async {
            let id = device_id(self.resources.directory.join("device.json"), &self.options).await?;
            let options = LoginOptions {
                get_device_id: Some(Arc::new(move || id.clone())),
                ..self.options.clone()
            };
            self.resources
                .models
                .login(provider, kind, self, &options)
                .await
        };
        if login.await.is_err() {
            let ending = self
                .logins
                .lock_inner()
                .active
                .as_ref()
                .filter(|active| active.id == self.id)
                .and_then(|active| active.ending);
            return match ending {
                Some(Ending::Cancelled) => ("cancelled", Some("Login cancelled.")),
                Some(Ending::Replaced) => ("cancelled", Some("Replaced by a new login.")),
                Some(Ending::Away) => (
                    "error",
                    Some(
                        "Login ended: no page of yours was connected for too long. Start it again.",
                    ),
                ),
                None if self.logins.lifetime.signal.is_cancelled() => {
                    ("cancelled", Some("Login cancelled: the server stopped."))
                }
                None if self.deadline_expired.load(Ordering::Acquire) => {
                    ("error", Some("Login expired. Start it again."))
                }
                None => (
                    "error",
                    Some("Provider login failed. Check the provider configuration and try again."),
                ),
            };
        }
        if (self.resources.publish)().await.is_err() {
            return (
                "success",
                Some("Signed in; resource refresh did not complete."),
            );
        }
        let refresh = self.resources.refresh(provider, &self.options).await;
        (
            "success",
            refresh.err().map(
                |_| "Signed in. Model-list refresh did not complete; use Refresh models to retry.",
            ),
        )
    }

    fn finish(&self, (status, message): (&str, Option<&str>)) {
        let mut inner = self.logins.lock_inner();
        if let Some(mut active) = inner
            .active
            .as_ref()
            .is_some_and(|active| active.id == self.id)
            .then(|| inner.active.take())
            .flatten()
        {
            if let Some(timer) = active.away_timer.take() {
                timer.abort();
            }
            inner.login["status"] = json!(status);
            inner.login["prompt"] = Value::Null;
            if let Some(message) = message {
                inner.login["message"] = json!(message);
            }
            let _ = self.logins.publish(&inner);
        }
    }
}

impl AuthInteraction for Interaction {
    fn prompt<'life0, 'async_trait>(
        &'life0 self,
        input: AuthPrompt,
    ) -> Pin<Box<dyn Future<Output = pi_rs_ai::Result<String>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let id = Uuid::new_v4().to_string();
            let (answer, received) = oneshot::channel();
            {
                let mut inner = self.logins.lock_inner();
                let active = inner
                    .active
                    .as_mut()
                    .filter(|active| active.id == self.id)
                    .ok_or(AiError::Aborted)?;
                if input.signal.is_cancelled() || self.options.signal.is_cancelled() {
                    return Err(AiError::Aborted);
                }
                let mut prompt = json!({"id":id,"message":input.message});
                let (kind, sensitive, options) = match input.kind {
                    PromptKind::Text => ("text", false, None),
                    PromptKind::Secret => ("secret", true, None),
                    PromptKind::ManualCode => ("manual_code", true, None),
                    PromptKind::Select(options) => {
                        if active.remote
                            && let Some(device) = options.iter().find(|option| {
                                matches!(option.id.as_str(), "device-code" | "device_code")
                            })
                        {
                            let id = device.id.clone();
                            drop(inner);
                            self.notify(AuthEvent::Info {
                                message: "Using device-code login on this remote connection."
                                    .into(),
                                links: Vec::new(),
                            });
                            return Ok(id);
                        }
                        prompt["options"] = json!(
                            options
                                .iter()
                                .map(|option| {
                                    let mut wire = json!({"id":option.id,"label":option.label});
                                    if let Some(description) = &option.description {
                                        wire["description"] = json!(description);
                                    }
                                    wire
                                })
                                .collect::<Vec<_>>()
                        );
                        ("select", false, Some(options))
                    }
                };
                prompt["type"] = json!(kind);
                if let Some(placeholder) = input.placeholder {
                    prompt["placeholder"] = json!(placeholder);
                }
                active.prompts.insert(
                    id.clone(),
                    PendingPrompt {
                        options,
                        sensitive,
                        answer,
                    },
                );
                inner.login["prompt"] = prompt;
                self.logins
                    .publish(&inner)
                    .map_err(|_| AiError::Auth("Could not publish the login prompt".into()))?;
            }
            let result = tokio::select! {
                result = received => result.map_err(|_| AiError::Aborted),
                _ = self.options.signal.cancelled() => Err(AiError::Aborted),
                _ = input.signal.cancelled() => Err(AiError::Aborted),
            };
            let mut inner = self.logins.lock_inner();
            if let Some(active) = inner.active.as_mut().filter(|active| active.id == self.id) {
                active.prompts.remove(&id);
                if inner.login["prompt"]["id"] == id {
                    inner.login["prompt"] = Value::Null;
                    let _ = self.logins.publish(&inner);
                }
            }
            result
        })
    }

    fn notify(&self, event: AuthEvent) {
        let mut inner = self.logins.lock_inner();
        if self.options.signal.is_cancelled()
            || inner
                .active
                .as_ref()
                .is_none_or(|active| active.id != self.id)
        {
            return;
        }
        let event = match event {
            AuthEvent::Info { message, links } => {
                let mut event = json!({"type":"info","message":message});
                if !links.is_empty() {
                    event["links"] = json!(
                        links
                            .into_iter()
                            .map(|(url, label)| {
                                let mut link = json!({"url":url});
                                if let Some(label) = label {
                                    link["label"] = json!(label);
                                }
                                link
                            })
                            .collect::<Vec<_>>()
                    );
                }
                event
            }
            AuthEvent::AuthUrl { url, instructions } => {
                process_logs().add_secret(&url);
                let mut event = json!({"type":"auth_url","url":url});
                if let Some(instructions) = instructions {
                    event["instructions"] = json!(instructions);
                }
                event
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                interval_seconds,
                expires_in_seconds,
                open_browser_on_confirm,
                copy_code_on_confirm,
            } => {
                process_logs().add_secret(&user_code);
                let mut event = json!({"type":"device_code","userCode":user_code,"verificationUri":verification_uri,
                    "intervalSeconds":interval_seconds,"openBrowserOnConfirm":open_browser_on_confirm,"copyCodeOnConfirm":copy_code_on_confirm});
                if let Some(expires) = expires_in_seconds {
                    event["expiresInSeconds"] = json!(expires);
                }
                event
            }
            AuthEvent::Progress(message) => json!({"type":"progress","message":message}),
        };
        if let Some(notices) = inner.login["notices"].as_array_mut() {
            notices.push(json!({"id":notices.len(),"event":event}));
            let _ = self.logins.publish(&inner);
        }
    }
}

fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .ok_or_else(|| invalid(&format!("Missing {key}")))
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

fn owned<'a>(inner: &'a mut Inner, args: &Value) -> Result<&'a mut Attempt> {
    let owner = text(args, "owner")?;
    let id = text(args, "attemptId")?;
    inner
        .active
        .as_mut()
        .filter(|active| active.id == id && active.owner == owner)
        .ok_or_else(|| invalid("Login attempt is no longer active for this connection"))
}

async fn wait(mut done: watch::Receiver<bool>) {
    let _ = done.wait_for(|done| *done).await;
}

async fn device_id(path: PathBuf, options: &LoginOptions) -> pi_rs_ai::Result<String> {
    use std::fs;
    let directory = path
        .parent()
        .ok_or_else(|| AiError::Auth("Missing device identity directory".into()))?;
    fs::create_dir_all(directory)?;
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let guard = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    loop {
        tokio::select! {
            biased;
            _ = options.signal.cancelled() => return Err(AiError::Aborted),
            result = async { guard.try_lock() } => match result {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => (),
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let data: Value = match fs::read(&path) {
        Ok(content) => serde_json::from_slice(&content)
            .map_err(|_| AiError::Auth("Invalid device identity file".into()))?,
        Err(error) if error.kind() == ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    if !data.is_object() {
        return Err(AiError::Auth("Invalid device identity file".into()));
    }
    if let Some(id) = data["id"].as_str().filter(|id| !id.is_empty()) {
        return Ok(id.to_owned());
    }
    let id = Uuid::new_v4().to_string();
    let mut pending = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(pending.as_file_mut(), &json!({"id":id})).map_err(io::Error::other)?;
    pending.persist(&path).map_err(|error| error.error)?;
    Ok(id)
}
