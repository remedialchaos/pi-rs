use super::*;
use indexmap::IndexSet;

const META_DEVICE: &str = "https://auth.meta.com/oidc/device/authorization/";
const META_TOKEN: &str = "https://auth.meta.com/oidc/device/token/";
const XAI_DEVICE: &str = "https://auth.x.ai/oauth2/device/code";
const XAI_TOKEN: &str = "https://auth.x.ai/oauth2/token";
const COPILOT_LABEL: &str = "GitHubCopilotChat/0.35.0";
const COPILOT_DEFAULT: &str = "https://api.individual.githubcopilot.com";
const COPILOT_HEADERS: [(&str, &str); 4] = [
    ("User-Agent", COPILOT_LABEL),
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];
fn kimi_endpoint(path: &str) -> String {
    let host = std::env::var("KIMI_CODE_OAUTH_HOST")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("KIMI_OAUTH_HOST")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "https://auth.kimi.com".into());
    format!("{}{path}", host.trim_end_matches('/'))
}
fn base_error(label: &str, reply: &Reply) -> Error {
    let detail = reply.detail();
    oauth(format!(
        "{label} with status {}{}",
        reply.status.as_u16(),
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    ))
}
impl BuiltinOAuth {
    pub(super) async fn device_login(
        &self,
        ui: &dyn AuthInteraction,
        opts: &LoginOptions,
        flow: Flow,
    ) -> Result<OAuthCredential> {
        let signal = &opts.signal;
        let kimi_device;
        let kimi_token;
        let (name, client, device_url, token_url) = match flow {
            Flow::Kimi => {
                kimi_device = kimi_endpoint("/api/oauth/device_authorization");
                kimi_token = kimi_endpoint("/api/oauth/token");
                ("Kimi Code", KIMI, kimi_device.as_str(), kimi_token.as_str())
            }
            Flow::Meta => ("Meta", META, META_DEVICE, META_TOKEN),
            Flow::Xai => ("xAI", XAI, XAI_DEVICE, XAI_TOKEN),
            _ => return Err(oauth("Invalid device flow")),
        };
        let mut fields = vec![("client_id", client)];
        if flow == Flow::Xai {
            fields.extend([
                (
                    "scope",
                    "openid profile email offline_access grok-cli:access api:access",
                ),
                ("referrer", "pi"),
            ]);
        }
        let reply = self.form(device_url, &fields, signal).await?;
        if !reply.ok() {
            return Err(base_error(
                &format!("{name} device authorization failed"),
                &reply,
            ));
        }
        let body = reply.body;
        let code = field(&body, "device_code", name)?;
        let user = field(&body, "user_code", name)?;
        let resolve_uri = |key| string(&body, key).and_then(|s| trusted_url(s, flow == Flow::Xai));
        let main = resolve_uri("verification_uri");
        let complete = resolve_uri("verification_uri_complete");
        let verify = match flow {
            Flow::Kimi if main.is_some() => complete,
            Flow::Xai if main.is_some() && string(&body, "verification_uri_complete").is_none() => {
                main
            }
            Flow::Xai if main.is_some() => complete,
            Flow::Meta => complete.or(main),
            _ => None,
        }
        .ok_or_else(|| {
            oauth(format!(
                "Untrusted verification URI in {name} OAuth response"
            ))
        })?;
        let interval = positive(&body, "interval");
        let ttl = match flow {
            Flow::Kimi => positive(&body, "expires_in").or(Some(900)),
            Flow::Xai => Some(
                positive(&body, "expires_in")
                    .ok_or_else(|| oauth("Invalid xAI OAuth response field: expires_in"))?,
            ),
            _ => positive(&body, "expires_in"),
        };
        device_event(ui, user, verify.as_str(), interval, ttl);
        let result = poll(interval, ttl, true, signal, || async {
            let reply = self
                .form(
                    token_url,
                    &[
                        ("client_id", client),
                        ("device_code", code),
                        ("grant_type", DEVICE_GRANT),
                    ],
                    signal,
                )
                .await?;
            if reply.ok() && string(&reply.body, "access_token").is_some() {
                return Ok(Poll::Ready(reply.body));
            }
            match reply.error_code() {
                Some("authorization_pending") => Ok(Poll::Pending),
                Some("slow_down") => Ok(Poll::Slow(positive(&reply.body, "interval"))),
                Some("expired_token") => Err(oauth(match flow {
                    Flow::Xai => "xAI device code expired",
                    Flow::Meta => "Meta device authorization expired. Please restart login.",
                    _ => "Kimi Code device authorization expired. Please restart login.",
                })),
                Some("access_denied" | "authorization_denied") => Err(oauth(match flow {
                    Flow::Xai => "xAI device authorization was denied",
                    Flow::Meta => "Meta login was denied.",
                    _ => "Kimi Code login was denied.",
                })),
                _ => Err(base_error(
                    &format!("{name} device token request failed"),
                    &reply,
                )),
            }
        })
        .await?;
        match flow {
            Flow::Meta => {
                progress(ui, "Enabling Meta Model API access...");
                self.mint_meta(field(&result, "access_token", "Meta")?, signal)
                    .await
            }
            Flow::Xai => token(&result, name, 300_000, None, Some(3600)),
            _ => token(&result, name, 0, None, None),
        }
    }
    async fn mint_meta(
        &self,
        identity: &str,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential> {
        let request = self
            .request(Method::POST, "https://api.meta.ai/muse-code/key")?
            .bearer_auth(identity)
            .header("x-api-version", "1.0.0")
            .json(&json!({}));
        let reply = execute(request, signal).await?;
        if matches!(reply.status.as_u16(), 401 | 403) {
            let extra = reply.detail();
            return Err(oauth(format!(
                "Meta session expired (status {}). Run `/login meta` to sign in again.{}",
                reply.status.as_u16(),
                if extra.is_empty() {
                    String::new()
                } else {
                    format!(": {extra}")
                }
            )));
        }
        if !reply.ok() {
            return Err(base_error("Meta API key mint failed", &reply));
        }
        let access = string(&reply.body, "api_key").ok_or_else(|| {
            let action = string(&reply.body, "action_url").and_then(|s| trusted_url(s, false));
            oauth(format!(
                "Meta did not issue an API key.{}",
                action
                    .map(|url| format!(" Complete setup at {url}"))
                    .unwrap_or_default()
            ))
        })?;
        Ok(OAuthCredential {
            access: access.into(),
            refresh: identity.into(),
            expires: expiry(86400, 0),
            details: Map::new(),
        })
    }
    pub(super) async fn device_refresh(
        &self,
        current: OAuthCredential,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential> {
        match self.flow {
            Flow::Xai => {
                let reply = self
                    .form(
                        XAI_TOKEN,
                        &[
                            ("grant_type", "refresh_token"),
                            ("client_id", XAI),
                            ("refresh_token", &current.refresh),
                        ],
                        signal,
                    )
                    .await?
                    .required("xAI OAuth token refresh failed")?;
                token(&reply, "xAI", 300_000, Some(&current.refresh), Some(3600))
            }
            Flow::Meta => self.mint_meta(&current.refresh, signal).await,
            Flow::Kimi => {
                let url = kimi_endpoint("/api/oauth/token");
                let fields = [
                    ("client_id", KIMI),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", current.refresh.as_str()),
                ];
                for attempt in 0..4 {
                    if attempt > 0 {
                        sleep(signal, Duration::from_secs(1 << (attempt - 1))).await?;
                    }
                    match self.form(&url, &fields, signal).await {
                        Err(e) if attempt < 3 && !matches!(e, Error::Aborted) => continue,
                        Err(e) => return Err(e),
                        Ok(reply) if reply.ok() => {
                            return token(&reply.body, "Kimi Code", 0, None, None);
                        }
                        Ok(reply)
                            if matches!(reply.status.as_u16(), 401 | 403)
                                || reply.error_code() == Some("invalid_grant") =>
                        {
                            let description = string(&reply.body, "error_description")
                                .map(|d| format!(": {d}"))
                                .unwrap_or_default();
                            return Err(oauth(format!(
                                "Kimi Code token refresh unauthorized (status {}){description}",
                                reply.status.as_u16()
                            )));
                        }
                        Ok(reply)
                            if attempt < 3
                                && (reply.status == reqwest::StatusCode::TOO_MANY_REQUESTS
                                    || reply.status.is_server_error()) =>
                        {
                            continue;
                        }
                        Ok(reply) => {
                            return Err(base_error("Kimi Code token refresh failed", &reply));
                        }
                    }
                }
                Err(oauth("Kimi Code token refresh failed"))
            }
            _ => Err(oauth("Invalid device credential type")),
        }
    }
    async fn copilot_request(
        &self,
        method: Method,
        url: &str,
        bearer: &str,
        mode: &str,
        retries: u32,
        signal: &CancellationToken,
    ) -> Result<Reply> {
        let time_limit = Duration::from_secs(5);
        let deadline = Instant::now() + time_limit;
        for attempt in 0..=retries {
            let mut builder = self.request(method.clone(), url)?.bearer_auth(bearer);
            for (header, value) in COPILOT_HEADERS {
                builder = builder.header(header, value);
            }
            match mode {
                "models" => builder = builder.header("X-GitHub-Api-Version", "2026-06-01"),
                "policy" => {
                    builder = builder
                        .header("openai-intent", "chat-policy")
                        .header("x-interaction-type", "chat-policy")
                        .json(&json!({"state":"enabled"}))
                }
                _ => {}
            }
            let time = deadline.saturating_duration_since(Instant::now());
            let response =
                abortable(signal, async { Ok(builder.timeout(time).send().await?) }).await?;
            let status = response.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < retries {
                let after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok());
                let delay = after.map_or(Some(Duration::from_millis(500u64 << attempt)), |value| {
                    value
                        .parse::<f64>()
                        .ok()
                        .and_then(|n| Duration::try_from_secs_f64(n.max(0.0)).ok())
                        .or_else(|| {
                            httpdate::parse_http_date(value).ok().map(|when| {
                                when.duration_since(std::time::SystemTime::now())
                                    .unwrap_or_default()
                            })
                        })
                });
                if let Some(delay) = delay
                    .filter(|delay| *delay < deadline.saturating_duration_since(Instant::now()))
                {
                    drop(response);
                    sleep(signal, delay).await?;
                    continue;
                }
            }
            let text = abortable(signal, async { Ok(response.text().await?) }).await?;
            let body = serde_json::from_str(&text).unwrap_or(Value::Null);
            return Ok(Reply { status, body, text });
        }
        Err(oauth("GitHub Copilot request retries exhausted"))
    }
    async fn copilot_mint(
        &self,
        github_token: &str,
        domain: Option<&str>,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential> {
        let url = format!(
            "https://api.{}/copilot_internal/v2/token",
            domain.unwrap_or("github.com")
        );
        let value = self
            .copilot_request(Method::GET, &url, github_token, "token", 0, signal)
            .await?
            .required("Copilot token request failed")?;
        let access = field(&value, "token", "Copilot")?.to_owned();
        let expires = positive(&value, "expires_at")
            .ok_or_else(|| oauth("Invalid Copilot token response fields"))?
            .saturating_mul(1000)
            .saturating_sub(300_000);
        let mut details = Map::new();
        if let Some(domain) = domain {
            details.insert("enterpriseUrl".into(), json!(domain));
        }
        Ok(OAuthCredential {
            access,
            refresh: github_token.into(),
            expires,
            details,
        })
    }
    async fn copilot_catalog(
        &self,
        mut creds: OAuthCredential,
        ui: Option<&dyn AuthInteraction>,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential> {
        let domain = enterprise(&creds);
        let base = copilot_base(&creds.access, domain.as_deref());
        let retries = if ui.is_some() { 2 } else { 0 };
        let body = self
            .copilot_request(
                Method::GET,
                &format!("{base}/models"),
                &creds.access,
                "models",
                retries,
                signal,
            )
            .await?
            .required("Copilot models request failed")?;
        let accounts = body["data"]
            .as_array()
            .ok_or_else(|| oauth("Invalid Copilot models response"))?;
        let models = accounts.iter().filter_map(|item| {
            let id = item["id"].as_str()?;
            if item["capabilities"]["supports"]["tool_calls"] == false {
                return None;
            }
            Some((
                id,
                item["model_picker_enabled"] == true,
                item["policy"]["state"].as_str().unwrap_or_default(),
            ))
        });
        let mut selected = models
            .clone()
            .filter_map(|(id, on, state)| (on && state != "disabled").then_some(id))
            .collect::<IndexSet<_>>();
        let use_fallback = selected.is_empty() && base == COPILOT_DEFAULT;
        if use_fallback {
            selected.extend(
                models
                    .clone()
                    .filter_map(|(id, _, state)| (state == "enabled").then_some(id)),
            );
        }
        if let Some(ui) = ui {
            let catalog = crate::catalog::Catalog::embedded()?;
            let candidates = models
                .filter(|(id, on, state)| {
                    (*on || use_fallback)
                        && *state == "unconfigured"
                        && catalog.get_model("github-copilot", id).is_some()
                })
                .collect::<Vec<_>>();
            if !candidates.is_empty() {
                progress(ui, "Enabling models...");
            }
            for (id, _, _) in candidates {
                let mut url = Url::parse(&base)?;
                url.path_segments_mut()
                    .map_err(|()| oauth("Invalid Copilot base URL"))?
                    .extend(["models", id, "policy"]);
                match self
                    .copilot_request(
                        Method::POST,
                        url.as_str(),
                        &creds.access,
                        "policy",
                        2,
                        signal,
                    )
                    .await
                {
                    Ok(reply) if reply.ok() => {
                        selected.insert(id);
                    }
                    Ok(reply) if reply.status == reqwest::StatusCode::TOO_MANY_REQUESTS => break,
                    Err(e) if signal.is_cancelled() => return Err(e),
                    _ => {}
                }
            }
        }
        let classifier_ids = accounts
            .iter()
            .filter_map(|item| {
                let id = item["id"].as_str()?;
                let kind = item["capabilities"]["type"].as_str()?;
                let state = item["policy"]["state"].as_str().unwrap_or_default();
                (kind == "decisions" && state != "disabled").then_some(id)
            })
            .collect::<IndexSet<_>>();
        creds
            .details
            .insert("availableModelIds".into(), json!(selected));
        creds
            .details
            .insert("availableClassifierModelIds".into(), json!(classifier_ids));
        Ok(creds)
    }
    pub(super) async fn copilot_refresh(
        &self,
        c: &OAuthCredential,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential> {
        let domain = enterprise(c);
        let credential = self
            .copilot_mint(&c.refresh, domain.as_deref(), signal)
            .await?;
        self.copilot_catalog(credential, None, signal).await
    }
    pub(super) async fn copilot(
        &self,
        ui: &dyn AuthInteraction,
        opts: &LoginOptions,
    ) -> Result<OAuthCredential> {
        let signal = &opts.signal;
        let input = prompt(
            ui,
            signal,
            "GitHub Enterprise URL/domain (blank for github.com)",
            PromptKind::Text,
            Some("company.ghe.com"),
        )
        .await?;
        let enterprise = domain(&input);
        if !input.trim().is_empty() && enterprise.is_none() {
            return Err(oauth("Invalid GitHub Enterprise URL/domain"));
        }
        let host = enterprise.as_deref().unwrap_or("github.com");
        let start = format!("https://{host}/login/device/code");
        let device = execute(
            self.request(Method::POST, &start)?
                .header("User-Agent", COPILOT_LABEL)
                .form(&[("client_id", COPILOT), ("scope", "read:user")]),
            signal,
        )
        .await?
        .required("GitHub Copilot device authorization failed")?;
        let code = field(&device, "device_code", "Copilot")?;
        let user = field(&device, "user_code", "Copilot")?;
        let uri = string(&device, "verification_uri")
            .and_then(|value| trusted_url(value, false))
            .ok_or_else(|| oauth("Untrusted verification_uri in device code response"))?;
        let ttl = Some(
            positive(&device, "expires_in")
                .ok_or_else(|| oauth("Invalid device code response fields"))?,
        );
        let interval = positive(&device, "interval");
        device_event(ui, user, uri.as_str(), interval, ttl);
        let access_url = format!("https://{host}/login/oauth/access_token");
        let github_token = poll(interval, ttl, true, signal, || async {
            let reply = execute(
                self.request(Method::POST, &access_url)?
                    .header("User-Agent", COPILOT_LABEL)
                    .form(&[
                        ("client_id", COPILOT),
                        ("device_code", code),
                        ("grant_type", DEVICE_GRANT),
                    ]),
                signal,
            )
            .await?
            .required("GitHub device flow failed")?;
            if let Some(access) = string(&reply, "access_token") {
                return Ok(Poll::Ready(access.to_owned()));
            }
            match string(&reply, "error") {
                Some("authorization_pending") => Ok(Poll::Pending),
                Some("slow_down") => Ok(Poll::Slow(positive(&reply, "interval"))),
                Some(error) => {
                    let suffix = string(&reply, "error_description")
                        .map(|s| format!(": {s}"))
                        .unwrap_or_default();
                    Err(oauth(format!("Device flow failed: {error}{suffix}")))
                }
                _ => Err(oauth("Invalid device token response")),
            }
        })
        .await?;
        let credential = self
            .copilot_mint(&github_token, enterprise.as_deref(), signal)
            .await?;
        self.copilot_catalog(credential, Some(ui), signal).await
    }
}
fn domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let url = if trimmed.contains("://") {
        Url::parse(trimmed)
    } else {
        Url::parse(&format!("https://{trimmed}"))
    }
    .ok()?;
    if !matches!(url.scheme(), "https" | "http") {
        return None;
    }
    url.host_str().map(str::to_owned)
}
pub(super) fn enterprise(credential: &OAuthCredential) -> Option<String> {
    credential
        .details
        .get("enterpriseUrl")
        .and_then(Value::as_str)
        .and_then(domain)
}
pub(super) fn copilot_base(token: &str, enterprise: Option<&str>) -> String {
    if let Some(host) = token
        .split(';')
        .find_map(|pair| pair.strip_prefix("proxy-ep="))
        .filter(|s| !s.is_empty())
    {
        let candidate = if let Some(host) = host.strip_prefix("proxy.") {
            format!("https://api.{host}")
        } else {
            format!("https://{host}")
        };
        if let Some(url) = trusted_url(&candidate, true)
            .filter(|u| u.path() == "/" && u.query().is_none() && u.fragment().is_none())
        {
            return url.as_str().trim_end_matches('/').into();
        }
    }
    enterprise
        .map(|host| format!("https://copilot-api.{host}"))
        .unwrap_or_else(|| COPILOT_DEFAULT.into())
}
