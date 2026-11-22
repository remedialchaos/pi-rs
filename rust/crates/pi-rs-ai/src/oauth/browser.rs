use super::*;

const ANTHROPIC_TOKEN: &str = "https://platform.claude.com/v1/oauth/token";
const CODEX_TOKEN: &str = "https://auth.openai.com/oauth/token";
const CHATGPT_TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const CALLBACK_PATH: &str = "/auth/callback";
const PASTE: &str =
    "Complete login in your browser, or paste the authorization code / redirect URL here:";
const REMOTE: &str = "Complete login in your browser. If the browser is on another machine, paste the final redirect URL here.";
impl BuiltinOAuth {
    pub(super) async fn anthropic(
        &self,
        ui: &dyn AuthInteraction,
        opts: &LoginOptions,
    ) -> Result<OAuthCredential> {
        const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
        let selection = choose(
            ui,
            opts,
            "Select Anthropic login method:",
            &[
                ("browser", "Browser login (default)"),
                ("copy_code", "Copy code login (headless)"),
            ],
        )
        .await?;
        let (verifier, challenge) = pkce();
        let mut server = None;
        let redirect = match selection.as_str() {
            "browser" => {
                let port = self.callback_port.unwrap_or(53692);
                let listener = callback(
                    &self.callback_host,
                    port,
                    "localhost",
                    "/callback",
                    Some(&verifier),
                    false,
                )
                .await;
                server = match listener {
                    Ok(c) => Some(c),
                    Err(_) if port != 0 => callback(
                        &self.callback_host,
                        0,
                        "localhost",
                        "/callback",
                        Some(&verifier),
                        false,
                    )
                    .await
                    .ok(),
                    Err(_) => None,
                };
                server
                    .as_ref()
                    .map(|s| s.url.clone())
                    .unwrap_or_else(|| format!("http://localhost:{port}/callback"))
            }
            "copy_code" => "https://platform.claude.com/oauth/code/callback".into(),
            _ => {
                return Err(oauth(format!(
                    "Unknown Anthropic login method: {selection}"
                )));
            }
        };
        let url = authorization(
            "https://claude.ai/oauth/authorize",
            &[
                ("code", "true"),
                ("client_id", ANTHROPIC),
                ("response_type", "code"),
                ("redirect_uri", &redirect),
                ("scope", SCOPES),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("state", &verifier),
            ],
        )?;
        let browser = selection == "browser";
        notify(
            ui,
            url,
            if browser {
                REMOTE
            } else {
                "Complete login in your browser, then copy the code Anthropic shows and paste it here."
            },
        );
        let grant = await_code(
            ui,
            opts,
            server.as_mut(),
            if browser {
                PASTE
            } else {
                "Paste the code Anthropic shows after you sign in:"
            },
            if browser { &redirect } else { "code#state" },
            Some(&verifier),
            false,
        )
        .await?;
        progress(ui, "Exchanging authorization code for tokens...");
        let body = json!({"grant_type":"authorization_code","client_id":ANTHROPIC,"code":grant.code,"state":grant.state.as_deref().unwrap_or(&verifier),"redirect_uri":redirect,"code_verifier":verifier});
        let result = self
            .post(ANTHROPIC_TOKEN, &body, &opts.signal)
            .await?
            .required("Token exchange request failed")?;
        token(&result, "Anthropic", 300_000, None, None)
    }
    pub(super) async fn anthropic_refresh(
        &self,
        refresh: &str,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential> {
        let body =
            json!({"grant_type":"refresh_token","client_id":ANTHROPIC,"refresh_token":refresh});
        let body = self
            .post(ANTHROPIC_TOKEN, &body, signal)
            .await?
            .required("Anthropic token refresh request failed")?;
        token(&body, "Anthropic", 300_000, None, None)
    }
    pub(super) async fn openrouter(
        &self,
        ui: &dyn AuthInteraction,
        opts: &LoginOptions,
    ) -> Result<OAuthCredential> {
        let (verifier, challenge) = pkce();
        let path = format!("/oauth/callback/{}", uuid::Uuid::new_v4());
        let mut server = callback(
            &self.callback_host,
            self.callback_port.unwrap_or_default(),
            "127.0.0.1",
            &path,
            None,
            false,
        )
        .await?;
        let redirect = server.url.clone();
        let url = authorization(
            "https://openrouter.ai/auth",
            &[
                ("callback_url", &redirect),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ],
        )?;
        progress(
            ui,
            &format!("Listening for OpenRouter OAuth callback on {redirect}"),
        );
        notify(
            ui,
            url,
            "Complete sign-in in your browser. If the browser is on another machine, paste the final redirect URL here.",
        );
        let grant=tokio::time::timeout(Duration::from_secs(300),await_code(ui,opts,Some(&mut server),"Complete sign-in in your browser, or paste the authorization code / redirect URL here:",&redirect,None,false)).await.map_err(|_|oauth("OpenRouter OAuth login timed out"))??;
        progress(ui, "Exchanging authorization code for an API key...");
        let reply = self
            .post(
                "https://openrouter.ai/api/v1/auth/keys",
                &json!({"code":grant.code,"code_verifier":verifier,"code_challenge_method":"S256"}),
                &opts.signal,
            )
            .await?
            .required("OpenRouter OAuth key exchange failed")?;
        let access = string(&reply, "key")
            .ok_or_else(|| oauth("OpenRouter OAuth response carries no \"key\""))?;
        Ok(OAuthCredential {
            access: access.into(),
            refresh: String::new(),
            expires: 9_007_199_254_740_991,
            details: Map::new(),
        })
    }
    pub(super) async fn chatgpt(
        &self,
        ui: &dyn AuthInteraction,
        opts: &LoginOptions,
    ) -> Result<OAuthCredential> {
        let id = opts
            .get_device_id
            .as_ref()
            .map(|f| f())
            .and_then(|s| uuid::Uuid::parse_str(&s).ok())
            .ok_or_else(|| {
                oauth("Sign in with ChatGPT requires a device ID (UUID) for this installation")
            })?;
        let host = format!("urn:uuid:{id}");
        let (verifier, challenge) = pkce();
        let state = random_value();
        let nonce = random_value();
        let port = self.callback_port.unwrap_or(1455);
        let mut server=callback(&self.callback_host,port,"127.0.0.1",CALLBACK_PATH,Some(&state),true).await.map_err(|e|{
            if matches!(&e,Error::Io(io) if io.kind()==std::io::ErrorKind::AddrInUse){oauth(format!("Port {port} is in use, probably by an unfinished login in another pi session or by the Codex CLI. Cancel that login and try again."))}else{e}
        })?;
        let redirect = server.url.clone();
        let scope = format!("openid profile email offline_access resource.invoke {CHATGPT_SCOPE}");
        let url = authorization(
            "https://auth.openai.com/api/accounts/authorize",
            &[
                ("client_id", "dynamic_agent_client"),
                (
                    "agent_name_hint",
                    opts.agent_name.as_deref().unwrap_or("Pi"),
                ),
                ("ext_agent_host_id", &host),
                ("response_type", "code"),
                ("redirect_uri", &redirect),
                ("resource", CHATGPT_RESOURCE),
                ("scope", &scope),
                ("state", &state),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("nonce", &nonce),
            ],
        )?;
        notify(
            ui,
            url,
            "Complete sign-in in your browser. If the callback does not complete, paste the final redirect URL here.",
        );
        let grant = await_code(
            ui,
            opts,
            Some(&mut server),
            "Complete login in your browser, or paste the final redirect URL here:",
            &redirect,
            Some(&state),
            true,
        )
        .await?;
        let client = grant.client_id.ok_or_else(|| {
            oauth("OpenAI OAuth registration callback did not contain an issued client ID")
        })?;
        progress(ui, "Exchanging authorization code for tokens...");
        let value = self
            .form(
                CHATGPT_TOKEN,
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", &client),
                    ("code", &grant.code),
                    ("code_verifier", &verifier),
                    ("redirect_uri", &redirect),
                    ("resource", CHATGPT_RESOURCE),
                ],
                &opts.signal,
            )
            .await?
            .required("OpenAI OAuth token request failed")?;
        if string(&value, "id_token").is_none() {
            return Err(oauth(
                "OpenAI OAuth token response did not contain an ID token",
            ));
        }
        chatgpt_token(&value, &client)
    }
    pub(super) async fn chatgpt_refresh(
        &self,
        current: &OAuthCredential,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential> {
        let client=current.details.get("clientId").and_then(Value::as_str).filter(|s|!s.trim().is_empty()).ok_or_else(||oauth("Stored OpenAI OAuth credential does not contain an issued client ID; reconnect ChatGPT"))?;
        let value = self
            .form(
                CHATGPT_TOKEN,
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", client),
                    ("refresh_token", &current.refresh),
                    ("resource", CHATGPT_RESOURCE),
                ],
                signal,
            )
            .await?
            .required("OpenAI OAuth token request failed")?;
        chatgpt_token(&value, client)
    }
    async fn codex_token(
        &self,
        fields: &[(&str, &str)],
        label: &str,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential> {
        let value = self
            .form(CODEX_TOKEN, fields, signal)
            .await?
            .required(label)?;
        let mut credential = token(&value, "OpenAI Codex", 0, None, None)?;
        let account = crate::codex::extract_account_id(&credential.access)
            .map_err(|_| oauth("Failed to extract accountId from token"))?;
        credential
            .details
            .insert("accountId".into(), json!(account));
        Ok(credential)
    }
    pub(super) async fn codex_refresh(
        &self,
        refresh: &str,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential> {
        self.codex_token(
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("client_id", CODEX),
            ],
            "OpenAI Codex token refresh failed",
            signal,
        )
        .await
    }
    pub(super) async fn codex(
        &self,
        ui: &dyn AuthInteraction,
        opts: &LoginOptions,
    ) -> Result<OAuthCredential> {
        let method = choose(
            ui,
            opts,
            "Select OpenAI Codex login method:",
            &[
                ("browser", "Browser login (default)"),
                ("device_code", "Device code login (headless)"),
            ],
        )
        .await?;
        let (code, verifier, redirect) = match method.as_str() {
            "browser" => {
                let (verifier, challenge) = pkce();
                let value = rand::random::<u128>();
                let state = format!("{value:032x}");
                let port = self.callback_port.unwrap_or(1455);
                let mut server = callback(
                    &self.callback_host,
                    port,
                    "localhost",
                    CALLBACK_PATH,
                    Some(&state),
                    false,
                )
                .await
                .ok();
                let redirect = server
                    .as_ref()
                    .map(|s| s.url.clone())
                    .unwrap_or_else(|| format!("http://localhost:{port}{CALLBACK_PATH}"));
                let url = authorization(
                    "https://auth.openai.com/oauth/authorize",
                    &[
                        ("response_type", "code"),
                        ("client_id", CODEX),
                        ("redirect_uri", &redirect),
                        ("scope", "openid profile email offline_access"),
                        ("code_challenge", &challenge),
                        ("code_challenge_method", "S256"),
                        ("state", &state),
                        ("id_token_add_organizations", "true"),
                        ("codex_cli_simplified_flow", "true"),
                        ("originator", opts.agent_name.as_deref().unwrap_or("pi")),
                    ],
                )?;
                notify(
                    ui,
                    url,
                    "A browser window should open. Complete login to finish.",
                );
                let code = await_code(
                    ui,
                    opts,
                    server.as_mut(),
                    PASTE,
                    &redirect,
                    Some(&state),
                    false,
                )
                .await?
                .code;
                (code, verifier, redirect)
            }
            "device_code" => {
                let response = self
                    .post(
                        "https://auth.openai.com/api/accounts/deviceauth/usercode",
                        &json!({"client_id":CODEX}),
                        &opts.signal,
                    )
                    .await?;
                if response.status == reqwest::StatusCode::NOT_FOUND {
                    return Err(oauth(
                        "OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL.",
                    ));
                }
                let response = response.required("OpenAI Codex device code request failed")?;
                let code = field(&response, "user_code", "OpenAI Codex")?;
                let id = field(&response, "device_auth_id", "OpenAI Codex")?;
                let interval = numeric(&response, "interval")
                    .or_else(|| {
                        response["interval"]
                            .as_str()
                            .and_then(|s| s.trim().parse().ok())
                    })
                    .ok_or_else(|| oauth("Invalid OpenAI Codex device code response"))?;
                device_event(
                    ui,
                    code,
                    "https://auth.openai.com/codex/device",
                    Some(interval),
                    Some(900),
                );
                let (code, verifier) =
                    poll(Some(interval), Some(900), false, &opts.signal, || async {
                        let reply = self
                            .post(
                                "https://auth.openai.com/api/accounts/deviceauth/token",
                                &json!({"device_auth_id":id,"user_code":code}),
                                &opts.signal,
                            )
                            .await?;
                        if reply.ok() {
                            return Ok(Poll::Ready((
                                field(&reply.body, "authorization_code", "OpenAI Codex")?
                                    .to_owned(),
                                field(&reply.body, "code_verifier", "OpenAI Codex")?.to_owned(),
                            )));
                        }
                        if matches!(reply.status.as_u16(), 403 | 404)
                            || reply.error_code() == Some("deviceauth_authorization_pending")
                        {
                            return Ok(Poll::Pending);
                        }
                        if reply.error_code() == Some("slow_down") {
                            return Ok(Poll::Slow(None));
                        }
                        reply
                            .required("OpenAI Codex device auth failed")
                            .map(|_| Poll::Pending)
                    })
                    .await?;
                (
                    code,
                    verifier,
                    "https://auth.openai.com/deviceauth/callback".into(),
                )
            }
            _ => {
                return Err(oauth(format!(
                    "Unknown OpenAI Codex login method: {method}"
                )));
            }
        };
        self.codex_token(
            &[
                ("grant_type", "authorization_code"),
                ("client_id", CODEX),
                ("code", &code),
                ("code_verifier", &verifier),
                ("redirect_uri", &redirect),
            ],
            "OpenAI Codex token exchange failed",
            &opts.signal,
        )
        .await
    }
    pub(super) async fn radius_token(
        &self,
        fields: &[(&str, &str)],
        signal: &CancellationToken,
    ) -> Result<OAuthCredential> {
        let reply = self
            .form(self.radius_url("/v1/oauth/token")?.as_str(), fields, signal)
            .await?
            .required("Radius OAuth token request failed")?;
        radius_token(&reply)
    }
    pub(super) async fn radius(
        &self,
        ui: &dyn AuthInteraction,
        opts: &LoginOptions,
    ) -> Result<OAuthCredential> {
        let method = choose(
            ui,
            opts,
            &format!("Sign in to {}:", self.radius_name),
            &[
                ("browser", "Sign in with browser (recommended)"),
                (
                    "device-code",
                    "Sign in with device code (when signing in from another device)",
                ),
            ],
        )
        .await?;
        if method == "device-code" {
            let device = self
                .form(
                    self.radius_url("/v1/oauth/device")?.as_str(),
                    &[("client_id", RADIUS), ("scope", RADIUS_SCOPE)],
                    &opts.signal,
                )
                .await?
                .required("Radius OAuth device authorization failed")?;
            let code = field(&device, "device_code", "Radius")?;
            let user = field(&device, "user_code", "Radius")?;
            let verify = string(&device, "verification_uri")
                .and_then(|s| trusted_url(s, false))
                .ok_or_else(|| {
                    oauth(
                        "Untrusted verification_uri in Radius OAuth device authorization response",
                    )
                })?;
            let ttl = positive(&device, "expires_in").ok_or_else(|| {
                oauth("Radius OAuth device authorization response is missing required fields")
            })?;
            let interval = positive(&device, "interval");
            device_event(ui, user, verify.as_str(), interval, Some(ttl));
            let url = self.radius_url("/v1/oauth/token")?;
            return poll(interval, Some(ttl), false, &opts.signal, || async {
                let reply = self
                    .form(
                        url.as_str(),
                        &[
                            ("grant_type", DEVICE_GRANT),
                            ("client_id", RADIUS),
                            ("device_code", code),
                        ],
                        &opts.signal,
                    )
                    .await?;
                if reply.ok() {
                    return Ok(Poll::Ready(radius_token(&reply.body)?));
                }
                match reply.error_code() {
                    Some("authorization_pending") => Ok(Poll::Pending),
                    Some("slow_down") => Ok(Poll::Slow(None)),
                    Some("expired_token") => Err(oauth("Device authorization expired.")),
                    Some("access_denied") => Err(oauth("Device authorization was denied.")),
                    _ => reply
                        .required("Radius OAuth token request failed")
                        .map(|_| Poll::Pending),
                }
            })
            .await;
        }
        if method != "browser" {
            return Err(oauth(format!(
                "Unknown {} sign-in method: {method}",
                self.radius_name
            )));
        }
        let discovery = execute(
            self.request(Method::GET, self.radius_url("/v1/oauth")?.as_str())?,
            &opts.signal,
        )
        .await?
        .required("Could not load Radius OAuth config")?;
        let auth = string(&discovery, "authorizationEndpoint")
            .and_then(|s| trusted_url(s, false))
            .ok_or_else(|| {
                oauth(format!(
                    "Invalid Radius OAuth config from {}",
                    self.radius_gateway
                ))
            })?;
        let (verifier, challenge) = pkce();
        let state = uuid::Uuid::new_v4().to_string();
        let mut c = callback(
            &self.callback_host,
            self.callback_port.unwrap_or(1456),
            "127.0.0.1",
            "/oauth/callback",
            Some(&state),
            false,
        )
        .await?;
        let redirect = c.url.clone();
        let url = authorization(
            auth.as_str(),
            &[
                ("response_type", "code"),
                ("client_id", RADIUS),
                ("redirect_uri", &redirect),
                ("scope", RADIUS_SCOPE),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("handoff", "url"),
                ("state", &state),
            ],
        )?;
        progress(ui, &format!("Listening for OAuth callback on {redirect}"));
        notify(ui, url, "Continue in your browser.");
        let code = abortable(&opts.signal, async {
            (&mut c.receiver)
                .await
                .map_err(|_| oauth("OAuth callback did not complete."))?
        })
        .await?
        .code;
        self.radius_token(
            &[
                ("grant_type", "authorization_code"),
                ("client_id", RADIUS),
                ("redirect_uri", &redirect),
                ("code", &code),
                ("code_verifier", &verifier),
            ],
            &opts.signal,
        )
        .await
    }
}
fn chatgpt_token(value: &Value, client: &str) -> Result<OAuthCredential> {
    let scopes = field(value, "scope", "OpenAI")?
        .split_whitespace()
        .collect::<Vec<_>>();
    if !scopes.contains(&CHATGPT_SCOPE) {
        return Err(oauth(format!(
            "OpenAI OAuth grant did not include {CHATGPT_SCOPE}"
        )));
    }
    let mut credentials = token(value, "OpenAI", 180_000, None, None)?;
    credentials.details.insert("clientId".into(), json!(client));
    credentials.details.insert("scopes".into(), json!(scopes));
    Ok(credentials)
}
fn radius_token(value: &Value) -> Result<OAuthCredential> {
    let mut credential = token(value, "Radius", 60_000, None, None)?;
    if let Some(scope) = string(value, "scope") {
        credential.details.insert("scope".into(), json!(scope));
    }
    Ok(credential)
}
