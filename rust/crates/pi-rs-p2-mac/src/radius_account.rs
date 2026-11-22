use crate::{
    Error, Result, http,
    oauth_loopback::{CLIENT_ID, Loopback, Pkce},
    write_json,
};
use reqwest::{Client, Method, Response, header};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use url::Url;

const TIMEOUT: Duration = Duration::from_secs(15);
type Document = Map<String, Value>;

#[derive(Clone)]
pub struct RadiusAccount {
    file: PathBuf,
    default_gateway: Url,
    client: Client,
    refresh: Arc<tokio::sync::Mutex<()>>,
    refused_refresh: Arc<Mutex<Option<String>>>,
}

pub fn allowed_endpoint(url: &Url) -> bool {
    url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some()
        && (url.scheme() == "https"
            || (url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost"))))
}

fn document(path: &Path) -> Option<Document> {
    serde_json::from_reader(fs::File::open(path).ok()?).ok()
}

pub(crate) fn string_at(path: &Path, key: &str) -> Option<String> {
    let Value::String(value) = document(path)?.remove(key)? else {
        return None;
    };
    Some(value)
}

fn credential(config: &Document) -> Option<&Document> {
    let cred = config.get("credential")?.as_object()?;
    cred.get("access")?.as_str()?;
    cred.get("refresh")?.as_str()?;
    Some(cred)
}

fn endpoint(base: &Url, parts: &[&str]) -> Result<Url> {
    let mut url = base.clone();
    url.set_query(None);
    url.set_fragment(None);
    url.path_segments_mut()
        .map_err(|()| Error::Radius(0, "invalid gateway URL".into()))?
        .pop_if_empty()
        .extend(parts);
    Ok(url)
}

fn same_gateway(gateway: &Url, config: &Document) -> bool {
    config
        .get("gateway")
        .and_then(Value::as_str)
        .and_then(|s| Url::parse(s).ok())
        .as_ref()
        == Some(gateway)
}

fn epoch_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

impl RadiusAccount {
    pub fn new(data_dir: &Path, default_gateway: Url) -> Result<Self> {
        if !allowed_endpoint(&default_gateway) {
            return Err(Error::Radius(0, "invalid gateway URL".into()));
        }
        Ok(Self {
            file: data_dir.join("radius.json"),
            default_gateway,
            client: http(TIMEOUT)?,
            refresh: Arc::default(),
            refused_refresh: Arc::default(),
        })
    }
    fn gateway_for(&self, doc: &Document) -> Url {
        doc.get("gateway")
            .and_then(Value::as_str)
            .and_then(|s| Url::parse(s).ok())
            .filter(allowed_endpoint)
            .unwrap_or_else(|| self.default_gateway.clone())
    }
    pub fn gateway(&self) -> Url {
        document(&self.file).map_or_else(
            || self.default_gateway.clone(),
            |doc| self.gateway_for(&doc),
        )
    }
    pub fn signed_in(&self) -> bool {
        let Some(config) = document(&self.file) else {
            return false;
        };
        let Some(refresh) = credential(&config)
            .and_then(|cred| cred.get("refresh"))
            .and_then(Value::as_str)
        else {
            return false;
        };
        self.refused_refresh
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref()
            != Some(refresh)
    }
    pub fn sign_out(&self) -> Result<()> {
        match fs::remove_file(&self.file) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        *self
            .refused_refresh
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }
    pub async fn sign_in(
        &self,
        headless: bool,
        open_browser: impl FnOnce(&Url) -> Result<()>,
    ) -> Result<()> {
        let gateway = &self.default_gateway;
        let discovery = self
            .client
            .get(endpoint(gateway, &["v1", "oauth"])?)
            .header(header::ACCEPT, "application/json")
            .send()
            .await?;
        let discovery = object(discovery).await?;
        let authorize = discovery
            .get("authorizationEndpoint")
            .and_then(Value::as_str)
            .and_then(|s| Url::parse(s).ok())
            .filter(allowed_endpoint)
            .ok_or_else(|| Error::Radius(0, "no authorization endpoint".into()))?;
        let pkce = Pkce::generate()?;
        let loopback = Loopback::start(&pkce).await?;
        let redirect = loopback.redirect.clone();
        let authorize = pkce.authorize(authorize, &redirect);
        let timeout = Duration::from_secs(600);
        let code = if headless {
            let auth_origin = authorize.origin();
            let callback = redirect.clone();
            let client = Client::builder()
                .no_proxy()
                .timeout(TIMEOUT)
                .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                    if attempt.previous().len() >= 10 {
                        return attempt.error("too many authorization redirects");
                    }
                    if attempt.url().origin() == auth_origin
                        || (attempt.url().origin() == callback.origin()
                            && attempt.url().path() == callback.path())
                    {
                        attempt.follow()
                    } else {
                        attempt.error("unexpected headless authorization redirect")
                    }
                }))
                .build()?;
            let request = client.get(authorize).send();
            let callback = loopback.callback(timeout);
            tokio::pin!(callback);
            tokio::select! {
                code = &mut callback => code?,
                result = request => { result?.error_for_status()?; callback.await? }
            }
        } else {
            open_browser(&authorize)?;
            loopback.callback(timeout).await?
        };
        let cred = self
            .token(
                gateway,
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", CLIENT_ID),
                    ("redirect_uri", redirect.as_str()),
                    ("code", &code),
                    ("code_verifier", &pkce.verifier),
                ],
            )
            .await?;
        let mut doc = document(&self.file)
            .filter(|doc| same_gateway(gateway, doc))
            .unwrap_or_else(|| {
                let mut doc = Document::new();
                doc.insert("serverId".into(), json!(uuid::Uuid::new_v4()));
                doc
            });
        let gateway = gateway.as_str().trim_end_matches('/');
        doc.insert("gateway".into(), json!(gateway));
        doc.insert("credential".into(), cred);
        write_json(&self.file, &doc)?;
        *self
            .refused_refresh
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }
    pub async fn servers(&self) -> Result<Value> {
        Ok(self
            .authorized(Method::GET, &["v1", "pi", "servers"], None)
            .await?
            .json()
            .await?)
    }
    pub async fn open_url(&self, org: &str, server: &str, path: &str) -> Result<Url> {
        #[derive(Deserialize)]
        struct Grant {
            url: String,
        }
        let res = self
            .authorized(
                Method::POST,
                &["v1", "pi", "servers", org, server, "open"],
                Some(path),
            )
            .await?;
        let grant = res
            .json::<Grant>()
            .await
            .ok()
            .and_then(|grant| Url::parse(&grant.url).ok())
            .filter(allowed_endpoint);
        grant.ok_or_else(|| Error::Radius(200, "no grant URL".into()))
    }
    async fn authorized(
        &self,
        method: Method,
        parts: &[&str],
        path: Option<&str>,
    ) -> Result<Response> {
        let (mut gateway, mut access) = self.access(None).await?;
        for retry in [false, true] {
            let mut url = endpoint(&gateway, parts)?;
            if let Some(path) = path {
                url.query_pairs_mut().append_pair("path", path);
            }
            let response = self
                .client
                .request(method.clone(), url)
                .bearer_auth(&access)
                .header(header::ACCEPT, "application/json")
                .send()
                .await?;
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            if status.as_u16() == 401 && !retry {
                (gateway, access) = self.access(Some(&access)).await?;
                continue;
            }
            let code = status.as_u16();
            let body = response.json::<Value>().await.ok();
            let message = body
                .as_ref()
                .and_then(|b| b["error"].as_str())
                .unwrap_or("error");
            if code == 401 {
                return Err(Error::SignedOut(message.into()));
            }
            if code == 404 && message == "pi_shell_not_enabled" {
                return Err(Error::NotEnabled);
            }
            return Err(Error::Radius(code, message.into()));
        }
        Err(Error::SignedOut("retry exhausted".into()))
    }
    async fn access(&self, rejected: Option<&str>) -> Result<(Url, String)> {
        let (gateway, access, expires, _) = self.current()?;
        if rejected.map_or(expires - epoch_ms() > 60_000.0, |previous| {
            previous != access
        }) {
            return Ok((gateway, access));
        }
        let _guard = self.refresh.lock().await;
        let (gateway, access, expires, refresh) = self.current()?;
        if rejected.map_or(expires - epoch_ms() > 60_000.0, |previous| {
            previous != access
        }) {
            return Ok((gateway, access));
        }
        let fresh = match self
            .token(
                &gateway,
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", CLIENT_ID),
                    ("refresh_token", &refresh),
                ],
            )
            .await
        {
            Err(Error::Radius(status, reason)) if (400..500).contains(&status) => {
                *self
                    .refused_refresh
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(refresh);
                return Err(Error::SignedOut(reason));
            }
            result => result?,
        };
        let mut latest =
            document(&self.file).ok_or_else(|| Error::SignedOut("not signed in".into()))?;
        let latest_cred =
            credential(&latest).ok_or_else(|| Error::SignedOut("not signed in".into()))?;
        if self.gateway_for(&latest) != gateway || latest_cred["refresh"].as_str() != Some(&refresh)
        {
            let (gateway, access, _, _) = self.current()?;
            return Ok((gateway, access));
        }
        let token = fresh["access"]
            .as_str()
            .ok_or_else(|| Error::Radius(200, "incomplete token response".into()))?
            .to_owned();
        latest.insert("credential".into(), fresh);
        write_json(&self.file, &latest)?;
        Ok((gateway, token))
    }
    fn current(&self) -> Result<(Url, String, f64, String)> {
        let signed_out = || Error::SignedOut("not signed in".into());
        let config = document(&self.file).ok_or_else(signed_out)?;
        let cred = credential(&config).ok_or_else(signed_out)?;
        let refresh = cred["refresh"].as_str().ok_or_else(signed_out)?;
        if self
            .refused_refresh
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref()
            == Some(refresh)
        {
            return Err(Error::SignedOut("credential refused".into()));
        }
        Ok((
            self.gateway_for(&config),
            cred["access"].as_str().ok_or_else(signed_out)?.into(),
            cred.get("expires").and_then(Value::as_f64).unwrap_or(0.0),
            refresh.into(),
        ))
    }
    async fn token(&self, gateway: &Url, form: &[(&str, &str)]) -> Result<Value> {
        let res = self
            .client
            .post(endpoint(gateway, &["v1", "oauth", "token"])?)
            .header(header::ACCEPT, "application/json")
            .form(form)
            .send()
            .await?;
        let answer = object(res).await?;
        let bad = || Error::Radius(200, "incomplete token response".into());
        let access = answer
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(bad)?;
        let refresh = answer
            .get("refresh_token")
            .and_then(Value::as_str)
            .ok_or_else(bad)?;
        let expires = answer
            .get("expires_in")
            .and_then(Value::as_f64)
            .ok_or_else(bad)?;
        let mut credential = json!({"type":"oauth","access":access,"refresh":refresh,"expires":epoch_ms() + (expires - 60.0) * 1000.0});
        if let Some(scope) = answer.get("scope").and_then(Value::as_str) {
            credential["scope"] = json!(scope);
        }
        Ok(credential)
    }
}

async fn object(response: Response) -> Result<Document> {
    let status = response.status();
    let result = response.json::<Document>().await;
    match result {
        Ok(doc) if status.is_success() => Ok(doc),
        result => {
            let message = result
                .ok()
                .and_then(|mut doc| {
                    doc.remove("error_description")
                        .or_else(|| doc.remove("error"))
                })
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unexpected answer".into());
            Err(Error::Radius(status.as_u16(), message))
        }
    }
}
