//! The server's own Radius identity and the lifecycle of its outbound connection.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use pi_rs_ai::auth::{Credential, LoginOptions, OAuthCredential};
use pi_rs_chord::{Error, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Mutex, watch},
    task::JoinHandle,
};
use url::Url;

use super::{
    login::{invalid, oauth},
    relay::RelayAccess,
    tunnel::ResolveForward,
};

const TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_MARGIN_MS: u64 = 5 * 60_000;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Config {
    pub gateway: String,
    pub server_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_page: Option<String>,
    pub credential: Credential,
}

impl Config {
    fn oauth(&self) -> Result<&OAuthCredential> {
        match &self.credential {
            Credential::OAuth(credential) => Ok(credential),
            Credential::ApiKey(_) => Err(Error::Invalid("Invalid radius.json".into())),
        }
    }
}

pub(super) struct ConfigFile(PathBuf);

impl ConfigFile {
    pub fn new(directory: &Path) -> Self {
        Self(directory.join("radius.json"))
    }

    pub async fn read(&self) -> Result<Option<Config>> {
        let text = match tokio::fs::read_to_string(&self.0).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(invalid(error)),
        };
        let config: Config = serde_json::from_str(&text)
            .map_err(|_| Error::Invalid("Invalid radius.json".into()))?;
        config.oauth()?;
        Ok(Some(config))
    }

    pub async fn write(&self, config: &Config) -> Result<()> {
        let path = self.0.clone();
        let contents = serde_json::to_vec_pretty(config)?;
        tokio::task::spawn_blocking(move || {
            let directory = path
                .parent()
                .ok_or_else(|| Error::Invalid("No Radius data directory".into()))?;
            std::fs::create_dir_all(directory).map_err(invalid)?;
            let mut file = tempfile::NamedTempFile::new_in(directory).map_err(invalid)?;
            file.write_all(&contents).map_err(invalid)?;
            file.write_all(b"\n").map_err(invalid)?;
            file.as_file().sync_all().map_err(invalid)?;
            file.persist(path).map_err(invalid)?;
            Ok(())
        })
        .await
        .map_err(invalid)?
    }
}

#[derive(Clone)]
pub struct SyncAccess {
    pub gateway: String,
    pub server_id: String,
    pub token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    SignedOut,
    Connecting,
    Connected {
        server_id: String,
        organization_id: String,
        name: Option<String>,
    },
    Retrying {
        error: String,
    },
}

#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Details {
    pub name: Option<String>,
    pub shares: Option<Vec<String>>,
}

pub struct RadiusBrain {
    file: ConfigFile,
    pub(super) port: u16,
    pub(super) relay: RelayAccess,
    pub(super) http: reqwest::Client,
    pub(super) forward: ResolveForward,
    public_origin: watch::Sender<Option<String>>,
    details: watch::Sender<Details>,
    status: watch::Sender<Status>,
    shell_page: watch::Sender<Option<String>>,
    config_lock: Mutex<()>,
    account: Mutex<Option<(String, String)>>,
    login: LoginOptions,
}

#[derive(Deserialize)]
struct ContextResponse {
    actor: ContextActor,
    organization: ContextOrganization,
}
#[derive(Deserialize)]
struct ContextActor {
    principal_id: String,
}
#[derive(Deserialize)]
struct ContextOrganization {
    id: String,
}

impl RadiusBrain {
    pub(super) fn new(
        directory: &Path,
        port: u16,
        relay: RelayAccess,
        forward: ResolveForward,
    ) -> Result<Self> {
        Ok(Self {
            file: ConfigFile::new(directory),
            port,
            relay,
            forward,
            public_origin: watch::channel(None).0,
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(invalid)?,
            details: watch::channel(Details::default()).0,
            status: watch::channel(Status::SignedOut).0,
            shell_page: watch::channel(None).0,
            config_lock: Mutex::new(()),
            account: Mutex::new(None),
            login: LoginOptions::default(),
        })
    }

    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.subscribe()
    }

    pub fn rename(&self, name: Option<String>) {
        self.details.send_if_modified(|details| {
            if details.name == name {
                return false;
            }
            details.name = name;
            true
        });
    }

    pub fn report_shares(&self, shares: Vec<String>) {
        self.details.send_if_modified(|details| {
            if details.shares.as_ref() == Some(&shares) {
                return false;
            }
            details.shares = Some(shares);
            true
        });
    }

    pub fn public_origin(&self) -> Option<String> {
        self.public_origin.borrow().clone()
    }

    pub fn shell_page(&self) -> Option<String> {
        self.shell_page.borrow().clone()
    }

    pub async fn sync_access(&self) -> Result<SyncAccess> {
        let config = self.fresh_config().await?;
        Ok(SyncAccess {
            token: config.oauth()?.access.clone(),
            gateway: config.gateway,
            server_id: config.server_id,
        })
    }

    /// The Radius account this brain is signed in as, `<organization ID>/<principal ID>`.
    /// Cached per access token; failed lookups are not remembered.
    pub async fn account(&self) -> Result<String> {
        let access = self.sync_access().await?;
        let mut cached = self.account.lock().await;
        if let Some((token, account)) = cached.as_ref()
            && token == &access.token
        {
            return Ok(account.clone());
        }
        let gateway = Url::parse(&access.gateway).map_err(invalid)?;
        let signal = self.login.signal.child_token();
        let http = self.http.clone();
        let token = access.token.clone();
        let result = tokio::select! {
            _ = signal.cancelled() => Err(Error::Closed),
            res = async {
                let context = http
                    .get(gateway.join("/v1/context").map_err(invalid)?)
                    .bearer_auth(&token)
                    .header(reqwest::header::ACCEPT, "application/json")
                    .send()
                    .await
                    .map_err(invalid)?
                    .error_for_status()
                    .map_err(invalid)?
                    .json::<ContextResponse>()
                    .await
                    .map_err(invalid)?;
                Ok::<String, Error>(format!("{}/{}", context.organization.id, context.actor.principal_id))
            } => res,
        };
        match result {
            Ok(account) => {
                *cached = Some((access.token, account.clone()));
                Ok(account)
            }
            Err(error) => {
                *cached = None;
                Err(error)
            }
        }
    }

    pub(super) fn start(self: &Arc<Self>) -> JoinHandle<()> {
        let brain = self.clone();
        tokio::spawn(async move { brain.run().await })
    }

    pub(super) fn close(&self) {
        self.login.signal.cancel();
    }

    async fn run(self: &Arc<Self>) {
        let mut replace = true;
        let mut delay = Duration::from_secs(1);
        loop {
            let wait = match self.file.read().await {
                Ok(None) => {
                    self.status.send_replace(Status::SignedOut);
                    TIMEOUT
                }
                result => {
                    self.status.send_replace(Status::Connecting);
                    let attempt = async {
                        let config = result?
                            .ok_or_else(|| Error::Invalid("Not signed in to Radius".into()))?;
                        self.public_origin.send_replace(config.public_origin);
                        if let Some(page) = config.shell_page {
                            self.shell_page.send_if_modified(|current| {
                                if current.is_some() {
                                    return false;
                                }
                                *current = Some(current_shell_page(&page));
                                true
                            });
                        }
                        super::tunnel::connect(
                            self,
                            &self.details,
                            &self.status,
                            &mut replace,
                            &mut delay,
                        )
                        .await
                    };
                    let result = tokio::select! {
                        _ = self.login.signal.cancelled() => return,
                        result = attempt => result,
                    };
                    let error = result
                        .err()
                        .map_or_else(|| "Radius relay closed".into(), |error| error.to_string());
                    self.status.send_replace(Status::Retrying { error });
                    let wait = delay;
                    delay = (delay * 2).min(TIMEOUT);
                    wait
                }
            };
            tokio::select! {
                _ = self.login.signal.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    async fn fresh_config(&self) -> Result<Config> {
        let _guard = self.config_lock.lock().await;
        let missing = || Error::Invalid("Not signed in to Radius".into());
        let initial = self.file.read().await?.ok_or_else(missing)?;
        let now = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(invalid)?
                .as_millis(),
        )
        .map_err(invalid)?;
        if initial.oauth()?.expires.saturating_sub(now) > REFRESH_MARGIN_MS {
            return Ok(initial);
        }
        let provider = oauth(Url::parse(&initial.gateway).map_err(invalid)?)?;
        let signal = self.login.signal.child_token();
        let refreshed = tokio::time::timeout(
            TIMEOUT,
            provider.refresh(initial.oauth()?.clone(), signal.clone()),
        )
        .await;
        signal.cancel();
        let refreshed = refreshed.map_err(invalid)?.map_err(invalid)?;
        let mut latest = self.file.read().await?.ok_or_else(missing)?;
        if latest.server_id == initial.server_id
            && latest.gateway == initial.gateway
            && latest.credential == initial.credential
        {
            latest.credential = Credential::OAuth(refreshed);
            self.file.write(&latest).await?;
        }
        Ok(latest)
    }

    pub(super) async fn remember(
        &self,
        identity: &SyncAccess,
        origin: String,
        page: String,
    ) -> Result<()> {
        if self.public_origin.borrow().as_ref() == Some(&origin)
            && self.shell_page.borrow().as_ref() == Some(&page)
        {
            return Ok(());
        }
        let _guard = self.config_lock.lock().await;
        if let Some(mut config) = self.file.read().await?
            && config.server_id == identity.server_id
            && config.gateway == identity.gateway
        {
            if config.public_origin.as_ref() != Some(&origin)
                || config.shell_page.as_ref() != Some(&page)
            {
                config.public_origin = Some(origin.clone());
                config.shell_page = Some(page.clone());
                self.file.write(&config).await?;
            }
            self.public_origin.send_replace(Some(origin));
            self.shell_page.send_replace(Some(page));
        }
        Ok(())
    }
}

fn current_shell_page(page: &str) -> String {
    if let Some((base, path)) = page.split_once("/pi/brains/")
        && Url::parse(base).is_ok_and(|url| url.origin().ascii_serialization() == base)
    {
        return format!("{base}/pi/{path}");
    }
    page.to_owned()
}
