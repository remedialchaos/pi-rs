#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use pi_rs_agent::CancellationToken;
use pi_rs_ai::Result as AiResult;
use pi_rs_coding_agent::cli::auth_login::run_with_terminal;
use pi_rs_coding_agent::cli::auth_terminal::{LoginTerminal, ProcessLoginTerminal};
use pi_rs_coding_agent::utils::open_browser::try_open_web_url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const CHROMIUM_BIN: &str = "/usr/bin/chromium";

fn ensure_chromium_exists() -> TestResult {
    if !Path::new(CHROMIUM_BIN).exists() {
        return Err(format!("missing Chromium binary at {CHROMIUM_BIN}").into());
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct PendingGrant {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    state: String,
    scope: Option<String>,
}

#[derive(Default, Debug)]
struct MockOAuthState {
    discovery_hits: usize,
    dcr_hits: usize,
    authorize_hits: usize,
    token_hits: usize,
    device_hits: usize,
    radius_config_hits: usize,
    registered_clients: HashMap<String, Vec<String>>,
    pending_grants: HashMap<String, PendingGrant>,
    pkce_verified: bool,
    verified_state: Option<String>,
    issued_access_token: Option<String>,
    next_code_id: usize,
}

struct MockOAuthServer {
    addr: SocketAddr,
    state: Arc<Mutex<MockOAuthState>>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl MockOAuthServer {
    async fn start() -> TestResult<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(Mutex::new(MockOAuthState::default()));
        let server_state = Arc::clone(&state);
        let accept_task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let conn_state = Arc::clone(&server_state);
                tokio::spawn(async move {
                    let _ = handle_http_connection(stream, addr, conn_state).await;
                });
            }
        });
        Ok(Self {
            addr,
            state,
            accept_task,
        })
    }

    fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for MockOAuthServer {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

async fn handle_http_connection(
    mut stream: TcpStream,
    addr: SocketAddr,
    state: Arc<Mutex<MockOAuthState>>,
) -> TestResult {
    let mut reader = BufReader::new(&mut stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();

    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let key = name.trim().to_ascii_lowercase();
            let val = value.trim().to_owned();
            if key == "content-length" {
                content_length = val.parse::<usize>().unwrap_or(0);
            }
            headers.insert(key, val);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await?;
    }

    let origin = format!("http://{addr}");
    let full_url = Url::parse(&format!("{origin}{target}"))?;
    let path = full_url.path().to_owned();
    let response = dispatch_request(&method, &path, &full_url, &headers, &body, &origin, &state)?;

    stream.write_all(&response).await?;
    stream.flush().await?;
    Ok(())
}

fn http_response(status: &str, extra_headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in extra_headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn json_response(status: &str, value: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(value).unwrap_or_default();
    http_response(
        status,
        &[
            ("Content-Type", "application/json"),
            ("Cache-Control", "no-store"),
        ],
        &body,
    )
}

fn dispatch_request(
    method: &str,
    path: &str,
    url: &Url,
    headers: &HashMap<String, String>,
    body: &[u8],
    origin: &str,
    state: &Arc<Mutex<MockOAuthState>>,
) -> TestResult<Vec<u8>> {
    match (method, path) {
        (
            "GET",
            "/.well-known/oauth-protected-resource" | "/.well-known/oauth-protected-resource/mcp",
        ) => Ok(json_response(
            "200 OK",
            &json!({
                "resource": format!("{origin}/mcp"),
                "authorization_servers": [origin],
                "scopes_supported": ["mcp:tools"],
            }),
        )),
        ("GET", "/.well-known/oauth-authorization-server") => {
            state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .discovery_hits += 1;
            Ok(json_response(
                "200 OK",
                &json!({
                    "issuer": origin,
                    "authorization_endpoint": format!("{origin}/authorize"),
                    "token_endpoint": format!("{origin}/token"),
                    "registration_endpoint": format!("{origin}/register"),
                    "response_types_supported": ["code"],
                    "grant_types_supported": ["authorization_code", "refresh_token"],
                    "code_challenge_methods_supported": ["S256"],
                    "token_endpoint_auth_methods_supported": ["none"],
                    "authorization_response_iss_parameter_supported": true,
                }),
            ))
        }
        ("GET", "/v1/oauth") => {
            state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .discovery_hits += 1;
            Ok(json_response(
                "200 OK",
                &json!({
                    "authorizationEndpoint": format!("{origin}/authorize"),
                }),
            ))
        }
        ("POST", "/register") => {
            let payload: Value = serde_json::from_slice(body)?;
            if payload["application_type"].as_str() != Some("native") {
                return Ok(json_response(
                    "400 Bad Request",
                    &json!({"error": "invalid_client_metadata"}),
                ));
            }
            let redirects: Vec<String> = payload["redirect_uris"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect();
            let client_id = "loopback-dcr-client".to_owned();
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            guard.dcr_hits += 1;
            guard
                .registered_clients
                .insert(client_id.clone(), redirects.clone());
            Ok(json_response(
                "201 Created",
                &json!({
                    "client_id": client_id,
                    "redirect_uris": redirects,
                    "token_endpoint_auth_method": "none",
                }),
            ))
        }
        ("GET", "/authorize") => {
            let params: HashMap<String, String> = url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            let response_type = params.get("response_type").map(String::as_str);
            let method = params.get("code_challenge_method").map(String::as_str);
            let challenge = params.get("code_challenge").cloned().unwrap_or_default();
            let state_param = params.get("state").cloned().unwrap_or_default();
            let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();
            let client_id = params.get("client_id").cloned().unwrap_or_default();
            if response_type != Some("code")
                || method != Some("S256")
                || challenge.is_empty()
                || state_param.is_empty()
                || redirect_uri.is_empty()
                || client_id.is_empty()
            {
                return Ok(http_response(
                    "400 Bad Request",
                    &[("Content-Type", "text/plain")],
                    b"invalid authorize request",
                ));
            }
            let code = {
                let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
                guard.authorize_hits += 1;
                guard.next_code_id += 1;
                let code = format!("auth-code-{}", guard.next_code_id);
                guard.pending_grants.insert(
                    code.clone(),
                    PendingGrant {
                        client_id,
                        redirect_uri: redirect_uri.clone(),
                        code_challenge: challenge,
                        state: state_param.clone(),
                        scope: params.get("scope").cloned(),
                    },
                );
                code
            };
            let mut location = Url::parse(&redirect_uri)?;
            location
                .query_pairs_mut()
                .append_pair("code", &code)
                .append_pair("state", &state_param)
                .append_pair("iss", origin);
            Ok(http_response(
                "302 Found",
                &[
                    ("Location", location.as_str()),
                    ("Cache-Control", "no-store"),
                ],
                b"",
            ))
        }
        ("POST", "/token" | "/v1/oauth/token") => {
            let fields: HashMap<String, String> =
                url::form_urlencoded::parse(body).into_owned().collect();
            let grant_type = fields.get("grant_type").map(String::as_str);
            match grant_type {
                Some("authorization_code") => {
                    let code = fields.get("code").map(String::as_str).unwrap_or_default();
                    let verifier = fields
                        .get("code_verifier")
                        .map(String::as_str)
                        .unwrap_or_default();
                    let redirect_uri = fields
                        .get("redirect_uri")
                        .map(String::as_str)
                        .unwrap_or_default();
                    let client_id = fields
                        .get("client_id")
                        .map(String::as_str)
                        .unwrap_or_default();
                    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
                    guard.token_hits += 1;
                    let Some(grant) = guard.pending_grants.remove(code) else {
                        return Ok(json_response(
                            "400 Bad Request",
                            &json!({"error": "invalid_grant", "error_description": "unknown code"}),
                        ));
                    };
                    let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
                    if computed != grant.code_challenge
                        || redirect_uri != grant.redirect_uri
                        || (!client_id.is_empty() && client_id != grant.client_id)
                    {
                        return Ok(json_response(
                            "400 Bad Request",
                            &json!({"error": "invalid_grant", "error_description": "PKCE or redirect mismatch"}),
                        ));
                    }
                    guard.pkce_verified = true;
                    guard.verified_state = Some(grant.state);
                    guard.issued_access_token = Some("loopback-access-token".to_owned());
                    Ok(json_response(
                        "200 OK",
                        &json!({
                            "access_token": "loopback-access-token",
                            "refresh_token": "loopback-refresh-token",
                            "token_type": "Bearer",
                            "expires_in": 3600,
                            "scope": grant.scope.unwrap_or_else(|| "mcp:tools".to_owned()),
                        }),
                    ))
                }
                Some("urn:ietf:params:oauth:grant-type:device_code") => {
                    let device_code = fields
                        .get("device_code")
                        .map(String::as_str)
                        .unwrap_or_default();
                    if device_code != "dev-code-1" {
                        return Ok(json_response(
                            "400 Bad Request",
                            &json!({"error": "invalid_grant"}),
                        ));
                    }
                    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
                    guard.token_hits += 1;
                    guard.issued_access_token = Some("device-access-token".to_owned());
                    Ok(json_response(
                        "200 OK",
                        &json!({
                            "access_token": "device-access-token",
                            "refresh_token": "device-refresh-token",
                            "token_type": "Bearer",
                            "expires_in": 3600,
                            "scope": "gateway offline_access",
                        }),
                    ))
                }
                _ => Ok(json_response(
                    "400 Bad Request",
                    &json!({"error": "unsupported_grant_type"}),
                )),
            }
        }
        ("POST", "/v1/oauth/device") => {
            state.lock().unwrap_or_else(|e| e.into_inner()).device_hits += 1;
            Ok(json_response(
                "200 OK",
                &json!({
                    "device_code": "dev-code-1",
                    "user_code": "ABCD-EFGH",
                    "verification_uri": format!("{origin}/device"),
                    "expires_in": 300,
                    "interval": 1,
                }),
            ))
        }
        ("GET", "/v1/config") => {
            let auth = headers.get("authorization").map(String::as_str);
            if auth != Some("Bearer loopback-access-token")
                && auth != Some("Bearer device-access-token")
            {
                return Ok(http_response(
                    "401 Unauthorized",
                    &[("Content-Type", "text/plain")],
                    b"Unauthorized",
                ));
            }
            state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .radius_config_hits += 1;
            Ok(json_response(
                "200 OK",
                &json!({
                    "baseUrl": format!("{origin}/v1"),
                    "models": [],
                }),
            ))
        }
        ("DELETE", "/mcp") => Ok(http_response("200 OK", &[], b"")),
        ("POST", "/mcp") => {
            let auth = headers.get("authorization").map(String::as_str);
            if auth != Some("Bearer loopback-access-token") {
                let challenge = format!(
                    "Bearer resource_metadata=\"{origin}/.well-known/oauth-protected-resource/mcp\", scope=\"mcp:tools\""
                );
                return Ok(http_response(
                    "401 Unauthorized",
                    &[
                        ("WWW-Authenticate", &challenge),
                        ("Content-Type", "text/plain"),
                    ],
                    b"Unauthorized",
                ));
            }
            let message: Value = serde_json::from_slice(body)?;
            if message.get("id").is_none() {
                return Ok(http_response("202 Accepted", &[], b""));
            }
            let result = match message.get("method").and_then(Value::as_str) {
                Some("initialize") => json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "loopback-oauth-mcp", "version": "1.0.0" },
                }),
                _ => json!({
                    "tools": [{ "name": "loopback_ping", "inputSchema": { "type": "object" } }],
                }),
            };
            Ok(json_response(
                "200 OK",
                &json!({
                    "jsonrpc": "2.0",
                    "id": message.get("id").cloned().unwrap_or(Value::Null),
                    "result": result,
                }),
            ))
        }
        _ => Ok(http_response(
            "404 Not Found",
            &[("Content-Type", "text/plain")],
            b"Not Found",
        )),
    }
}

struct ChromiumRun {
    url: String,
    dom: String,
    status_code: Option<i32>,
    stderr: String,
}

type ChromiumRunHandle = std::thread::JoinHandle<Result<ChromiumRun, String>>;
type ChromiumRunHandles = Arc<Mutex<Vec<ChromiumRunHandle>>>;

struct ChromiumLoginTerminal {
    inner: ProcessLoginTerminal,
    runs: ChromiumRunHandles,
    lines: Mutex<Vec<String>>,
    errors: Mutex<Vec<String>>,
}

impl ChromiumLoginTerminal {
    fn new(signal: CancellationToken, profile_root: PathBuf) -> Self {
        let runs: ChromiumRunHandles = Arc::default();
        let runs_handle = Arc::clone(&runs);
        let counter = Arc::new(AtomicUsize::new(0));
        let inner = ProcessLoginTerminal::new(signal).with_browser_opener(Arc::new(move |url| {
            let owned_url = url.to_owned();
            let idx = counter.fetch_add(1, Ordering::Relaxed);
            let profile = profile_root.join(format!("profile-{idx}"));
            let handle = std::thread::spawn(move || {
                std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
                let output = Command::new(CHROMIUM_BIN)
                    .args([
                        "--headless=new",
                        "--no-sandbox",
                        "--disable-gpu",
                        "--disable-dev-shm-usage",
                        "--no-first-run",
                        "--no-default-browser-check",
                    ])
                    .arg(format!("--user-data-dir={}", profile.display()))
                    .arg("--dump-dom")
                    .arg(&owned_url)
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| format!("failed to spawn chromium: {e}"))?;
                Ok(ChromiumRun {
                    url: owned_url,
                    dom: String::from_utf8_lossy(&output.stdout).into_owned(),
                    status_code: output.status.code(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                })
            });
            runs_handle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(handle);
            Ok(())
        }));
        Self {
            inner,
            runs,
            lines: Mutex::default(),
            errors: Mutex::default(),
        }
    }

    fn join_runs(&self) -> Result<Vec<ChromiumRun>, String> {
        let handles = {
            let mut guard = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *guard)
        };
        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            let run = handle
                .join()
                .map_err(|_| "chromium runner thread panicked".to_owned())??;
            results.push(run);
        }
        Ok(results)
    }
}

#[async_trait]
impl LoginTerminal for ChromiumLoginTerminal {
    fn interactive(&self) -> bool {
        true
    }

    fn write(&self, line: &str) {
        self.lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(line.to_owned());
    }

    fn error(&self, line: &str) {
        self.errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(line.to_owned());
    }

    fn open_url(&self, url: &str) {
        self.inner.open_url(url);
    }

    async fn prompt(
        &self,
        message: &str,
        secret: bool,
        signal: &CancellationToken,
    ) -> AiResult<String> {
        self.inner.prompt(message, secret, signal).await
    }

    async fn settle(&self) {
        self.inner.settle().await;
    }
}

#[cfg(unix)]
fn assert_owner_only_mode(path: &Path) -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "expected 0600 on {}", path.display());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pi_auth_login_radius_chromium_pkce_e2e() -> TestResult {
    ensure_chromium_exists()?;
    let server = MockOAuthServer::start().await?;
    let root = tempfile::tempdir()?;
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(&agent_dir)?;

    let models_json = json!({
        "providers": {
            "radius": {
                "oauth": "radius",
                "baseUrl": format!("{}/v1", server.origin())
            }
        }
    });
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::to_vec_pretty(&models_json)?,
    )?;

    let signal = CancellationToken::new();
    let terminal = ChromiumLoginTerminal::new(signal.clone(), root.path().join("chrome"));
    let code = tokio::time::timeout(
        Duration::from_secs(45),
        run_with_terminal(
            &["radius"],
            root.path(),
            &agent_dir,
            false,
            signal,
            &terminal,
        ),
    )
    .await?;

    let errors = terminal.errors.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(code, 0, "login errors: {errors:?}");
    drop(errors);

    let runs = terminal.join_runs().map_err(std::io::Error::other)?;
    assert_eq!(runs.len(), 1, "expected exactly 1 Chromium launch");
    let run = &runs[0];
    assert_eq!(run.status_code, Some(0), "chromium stderr: {}", run.stderr);
    assert!(
        run.url
            .starts_with(&format!("{}/authorize?", server.origin())),
        "unexpected authorize URL: {}",
        run.url
    );
    assert!(
        run.dom.contains("Authentication complete")
            && run.dom.contains("You can close this window."),
        "unexpected callback DOM: {}",
        run.dom
    );

    {
        let guard = server.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(guard.pkce_verified, "PKCE S256 verifier was not verified");
        assert!(
            guard
                .verified_state
                .as_deref()
                .is_some_and(|s| !s.is_empty()),
            "OAuth state was not verified"
        );
        assert!(
            guard.radius_config_hits >= 1,
            "expected /v1/config refresh with bearer token"
        );
    }

    let auth_path = agent_dir.join("auth.json");
    assert!(auth_path.is_file(), "auth.json must exist");
    #[cfg(unix)]
    assert_owner_only_mode(&auth_path)?;
    let persisted: Value = serde_json::from_slice(&std::fs::read(&auth_path)?)?;
    assert_eq!(persisted["radius"]["type"], "oauth");
    assert_eq!(persisted["radius"]["access"], "loopback-access-token");
    assert_eq!(persisted["radius"]["refresh"], "loopback-refresh-token");
    assert!(
        persisted["radius"]["expires"]
            .as_u64()
            .is_some_and(|exp| exp > 0)
    );

    let printed = Command::new(env!("CARGO_BIN_EXE_pi"))
        .env_clear()
        .env("PI_CODING_AGENT_DIR", &agent_dir)
        .args(["auth", "print-bearer-token", "--provider", "radius"])
        .output()?;
    assert_eq!(printed.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&printed.stdout),
        "loopback-access-token\n"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pi_mcp_login_chromium_pkce_via_pi_browser_cmd_e2e() -> TestResult {
    use std::os::unix::fs::PermissionsExt;

    ensure_chromium_exists()?;
    let server = MockOAuthServer::start().await?;
    let root = tempfile::tempdir()?;
    let agent_dir = root.path().join("agent");
    let chrome_profile = root.path().join("chrome-mcp");
    std::fs::create_dir_all(&chrome_profile)?;

    let dom_out = root.path().join("callback.dom.html");
    let dom_tmp = root.path().join("callback.dom.tmp");
    let err_out = root.path().join("chromium.stderr.log");
    let opener_script = root.path().join("open-chromium.sh");
    let script_body = format!(
        "#!/bin/sh\nset -eu\n\"{CHROMIUM_BIN}\" --headless=new --no-sandbox --disable-gpu --disable-dev-shm-usage --no-first-run --no-default-browser-check \"--user-data-dir={}\" --dump-dom \"$1\" > \"{}\" 2> \"{}\"\nmv \"{}\" \"{}\"\n",
        chrome_profile.display(),
        dom_tmp.display(),
        err_out.display(),
        dom_tmp.display(),
        dom_out.display(),
    );
    std::fs::write(&opener_script, script_body)?;
    std::fs::set_permissions(&opener_script, std::fs::Permissions::from_mode(0o755))?;

    let mcp_url = format!("{}/mcp", server.origin());
    let add_out = tokio::process::Command::new(env!("CARGO_BIN_EXE_pi"))
        .current_dir(root.path())
        .env_clear()
        .env("HOME", root.path())
        .env("USERPROFILE", root.path())
        .env("PI_CODING_AGENT_DIR", &agent_dir)
        .args(["mcp", "add", "loopback-oauth", "--url", &mcp_url])
        .output()
        .await?;
    assert_eq!(
        add_out.status.code(),
        Some(0),
        "mcp add stderr: {}",
        String::from_utf8_lossy(&add_out.stderr)
    );

    let login_out = tokio::time::timeout(
        Duration::from_secs(45),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_pi"))
            .current_dir(root.path())
            .env_clear()
            .env("HOME", root.path())
            .env("USERPROFILE", root.path())
            .env("PI_CODING_AGENT_DIR", &agent_dir)
            .env("PI_BROWSER_CMD", &opener_script)
            .args(["mcp", "login", "loopback-oauth", "--timeout", "30"])
            .stdin(Stdio::null())
            .output(),
    )
    .await??;

    assert_eq!(
        login_out.status.code(),
        Some(0),
        "mcp login stderr: {}",
        String::from_utf8_lossy(&login_out.stderr)
    );
    let stdout = String::from_utf8_lossy(&login_out.stdout);
    assert!(
        stdout.contains("Signed in to MCP server \"loopback-oauth\" (1 tools)."),
        "unexpected mcp login stdout: {stdout}"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while !dom_out.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(dom_out.exists(), "Chromium DOM output was not written");
    let dom = std::fs::read_to_string(&dom_out)?;
    assert!(
        dom.contains("Authorization complete. You may close this window."),
        "unexpected MCP callback DOM: {dom}"
    );

    {
        let guard = server.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(guard.discovery_hits >= 1, "expected AS metadata discovery");
        assert_eq!(guard.dcr_hits, 1, "expected dynamic client registration");
        assert_eq!(guard.authorize_hits, 1, "expected /authorize hit");
        assert!(guard.pkce_verified, "expected PKCE S256 verification");
        assert!(
            guard
                .verified_state
                .as_deref()
                .is_some_and(|s| !s.is_empty()),
            "expected state verification"
        );
    }

    let mcp_auth = agent_dir.join("mcp-auth.json");
    assert!(mcp_auth.is_file(), "mcp-auth.json must exist");
    assert_owner_only_mode(&mcp_auth)?;
    let persisted: Value = serde_json::from_slice(&std::fs::read(&mcp_auth)?)?;
    let key = format!("{}|{mcp_url}", pi_rs_mcp::mcp_namespace("loopback-oauth"));
    assert_eq!(
        persisted[&key]["tokens"]["access_token"],
        "loopback-access-token"
    );
    assert_eq!(
        persisted[&key]["tokens"]["refresh_token"],
        "loopback-refresh-token"
    );
    assert!(
        persisted[&key]["tokensExpireAt"]
            .as_f64()
            .is_some_and(|exp| exp > 0.0)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pi_auth_login_no_browser_and_unsafe_url_schemes_refused() -> TestResult {
    let server = MockOAuthServer::start().await?;
    let root = tempfile::tempdir()?;
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(&agent_dir)?;

    let models_json = json!({
        "providers": {
            "radius": {
                "oauth": "radius",
                "baseUrl": format!("{}/v1", server.origin())
            }
        }
    });
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::to_vec_pretty(&models_json)?,
    )?;

    let opener_calls = Arc::new(AtomicUsize::new(0));
    let calls_ref = Arc::clone(&opener_calls);
    let signal = CancellationToken::new();
    let inner =
        ProcessLoginTerminal::new(signal.clone()).with_browser_opener(Arc::new(move |_| {
            calls_ref.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));

    struct CountingTerminal {
        inner: ProcessLoginTerminal,
    }

    #[async_trait]
    impl LoginTerminal for CountingTerminal {
        fn interactive(&self) -> bool {
            true
        }
        fn write(&self, _line: &str) {}
        fn error(&self, _line: &str) {}
        fn open_url(&self, url: &str) {
            self.inner.open_url(url);
        }
        async fn prompt(
            &self,
            message: &str,
            secret: bool,
            signal: &CancellationToken,
        ) -> AiResult<String> {
            self.inner.prompt(message, secret, signal).await
        }
        async fn settle(&self) {
            self.inner.settle().await;
        }
    }

    let terminal = CountingTerminal { inner };
    let code = tokio::time::timeout(
        Duration::from_secs(30),
        run_with_terminal(
            &["radius", "--no-browser"],
            root.path(),
            &agent_dir,
            false,
            signal,
            &terminal,
        ),
    )
    .await?;
    assert_eq!(code, 0);
    assert_eq!(
        opener_calls.load(Ordering::SeqCst),
        0,
        "--no-browser must never invoke the browser opener"
    );

    let auth_path = agent_dir.join("auth.json");
    assert!(auth_path.is_file());
    #[cfg(unix)]
    assert_owner_only_mode(&auth_path)?;
    let persisted: Value = serde_json::from_slice(&std::fs::read(&auth_path)?)?;
    assert_eq!(persisted["radius"]["access"], "device-access-token");

    for unsafe_url in [
        "javascript:alert(1)",
        "JAVASCRIPT:void(0)",
        "file:///etc/passwd",
        "data:text/html,<script>alert(1)</script>",
    ] {
        terminal.open_url(unsafe_url);
        assert!(
            try_open_web_url(unsafe_url).is_err(),
            "unsafe URL should be refused: {unsafe_url}"
        );
    }
    assert_eq!(
        opener_calls.load(Ordering::SeqCst),
        0,
        "unsafe URL schemes must never reach the browser opener"
    );
    Ok(())
}
