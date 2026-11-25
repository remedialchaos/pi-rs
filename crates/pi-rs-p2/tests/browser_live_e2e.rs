#![forbid(unsafe_code)]

use std::{
    collections::HashMap,
    error::Error,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use pi_rs_p2::{
    hosts::server::{ServerOptions, start_server},
    plugins,
    runtime::local_owner::{LOCAL_CHALLENGE_PATH, LOCAL_CLAIM_PATH, local_owner_proof},
};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_tungstenite::{connect_async, tungstenite::Message};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const CHROMIUM_BIN: &str = "/usr/bin/chromium";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_csp_local_auth_and_all_48_plugins_work() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut options = ServerOptions::new(dir.path().to_owned());
    options.port = 0;
    options.fake = true;
    plugins::register_builtins(&mut options.registry)?;
    let server = start_server(options).await?;
    assert_eq!(server.plugins.len(), 48);

    let origin = format!("http://{}", server.address);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?;

    let health = client.get(format!("{origin}/health")).send().await?;
    assert_eq!(health.status(), reqwest::StatusCode::OK);
    assert_eq!(
        health.json::<Value>().await?,
        json!({"ok": true, "name": "p2"})
    );

    let unauth = client.get(format!("{origin}/")).send().await?;
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
    let unauth_csp = unauth
        .headers()
        .get(reqwest::header::CONTENT_SECURITY_POLICY)
        .and_then(|v| v.to_str().ok())
        .ok_or("missing CSP on unauthenticated sign-in page")?
        .to_owned();
    assert!(unauth_csp.contains("default-src 'none'"));
    assert!(unauth_csp.contains("script-src 'nonce-"));
    let sign_in_html = unauth.text().await?;
    assert!(sign_in_html.contains("Sign in to Pi"));

    let (cookie_name, cookie_value, set_cookie_raw) =
        claim_local_owner_cookie(&client, &origin, server.address.port(), dir.path()).await?;
    assert!(set_cookie_raw.contains("HttpOnly"));
    assert!(set_cookie_raw.contains("SameSite=Lax"));
    assert!(set_cookie_raw.contains("Path=/"));

    if let Some(base) = pi_rs_p2::hosts::web::browser_base() {
        let cookie_header = format!("{cookie_name}={cookie_value}");
        let authed = client
            .get(format!("{origin}/"))
            .header(reqwest::header::COOKIE, &cookie_header)
            .send()
            .await?;
        assert_eq!(authed.status(), reqwest::StatusCode::OK);
        let csp = authed
            .headers()
            .get(reqwest::header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .ok_or("missing CSP on authenticated page")?
            .to_owned();
        assert!(csp.contains("'wasm-unsafe-eval'"));
        let html = authed.text().await?;
        assert!(html.contains(&format!("data-web-version=\"{base}\"")));

        for (file, expected_mime) in [
            ("app.js", "text/javascript"),
            ("app_bg.wasm", "application/wasm"),
            ("p2.css", "text/css"),
        ] {
            let asset = client
                .get(format!("{origin}/_chord/web/{base}/{file}"))
                .header(reqwest::header::COOKIE, &cookie_header)
                .send()
                .await?;
            assert_eq!(asset.status(), reqwest::StatusCode::OK, "asset {file}");
            let ct = asset
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            assert!(
                ct.starts_with(expected_mime),
                "unexpected mime for {file}: {ct}"
            );
        }
    }

    server.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_p2_chromium_e2e() -> TestResult {
    if !Path::new(CHROMIUM_BIN).exists() {
        return Err(format!("missing Chromium binary at {CHROMIUM_BIN}").into());
    }
    let p2_bin = tokio::task::spawn_blocking(resolve_packaged_p2_binary).await??;
    tokio::time::timeout(Duration::from_secs(120), run_chromium_e2e(&p2_bin)).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_share_admission_and_expiration_chromium_e2e() -> TestResult {
    if !Path::new(CHROMIUM_BIN).exists() {
        return Err(format!("missing Chromium binary at {CHROMIUM_BIN}").into());
    }
    let p2_bin = tokio::task::spawn_blocking(resolve_packaged_p2_binary).await??;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_guest_share_expiration_e2e(&p2_bin),
    )
    .await?
}

async fn run_chromium_e2e(p2_bin: &Path) -> TestResult {
    let data_dir = tempfile::tempdir()?;
    let chromium_profile = tempfile::tempdir()?;

    let mut p2_proc = spawn_p2_server(p2_bin, data_dir.path())?;
    let mut p2_guard = ChildGuard::new(&mut p2_proc.child);
    let origin = format!("http://{}", p2_proc.address);
    let port = p2_proc.address.port();

    let chromium = ChromiumProcess::spawn(chromium_profile.path())?;
    let cdp = CdpClient::connect(&chromium.ws_url).await?;
    let session_id = cdp.attach_page().await?;
    cdp.enable_page_domains(&session_id).await?;

    // 1. Unauthenticated check: opening / without cookies shows sign-in, not the app.
    cdp.navigate(&session_id, &origin).await?;
    wait_until("sign-in page", Duration::from_secs(10), || async {
        let ready = cdp
            .eval_json(
                &session_id,
                r#"(() => ({
                    title: document.title,
                    hasForm: Boolean(document.getElementById('sign-in')),
                    hasRoot: Boolean(document.getElementById('root')),
                    text: document.body ? document.body.innerText : ''
                }))()"#,
            )
            .await?;
        let ok = ready["hasForm"].as_bool() == Some(true)
            && ready["hasRoot"].as_bool() == Some(false)
            && ready["text"]
                .as_str()
                .is_some_and(|t| t.contains("Sign in to Pi"));
        Ok(ok.then_some(ready))
    })
    .await?;

    // 2. Authenticate via local owner challenge/claim and inject cookie into Chromium.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?;
    let (cookie_name, cookie_value, _) =
        claim_local_owner_cookie(&http, &origin, port, data_dir.path()).await?;
    cdp.clear_telemetry();
    cdp.call(
        "Page.addScriptToEvaluateOnNewDocument",
        json!({
            "source": r#"
                window.__cspViolations = [];
                document.addEventListener('securitypolicyviolation', (e) => {
                    window.__cspViolations.push({
                        directive: e.violatedDirective,
                        blockedURI: e.blockedURI
                    });
                });
            "#
        }),
        Some(&session_id),
    )
    .await?;
    cdp.call(
        "Network.setCookie",
        json!({
            "name": cookie_name,
            "value": cookie_value,
            "url": origin,
            "path": "/",
            "httpOnly": true,
            "sameSite": "Lax"
        }),
        Some(&session_id),
    )
    .await?;

    // 3. Navigate to / as authenticated owner and wait for WebSocket Connected state.
    cdp.navigate(&session_id, &origin).await?;
    wait_for_connected(&cdp, &session_id).await?;
    assert_no_errors_and_assets_ok(&cdp, &session_id).await?;

    // 4. Click "New session" and verify sidebar session + enabled composer.
    let clicked = cdp
        .eval_json(
            &session_id,
            r#"(() => {
                const btn = Array.from(document.querySelectorAll('button'))
                    .find((b) => b.textContent.trim() === 'New session' && !b.disabled);
                if (!btn) return false;
                btn.click();
                return true;
            })()"#,
        )
        .await?;
    assert_eq!(clicked, json!(true), "New session button must be clickable");

    if let Err(err) = wait_until(
        "new session and enabled composer",
        Duration::from_secs(15),
        || async {
            let state = cdp
                .eval_json(
                    &session_id,
                    r#"(() => {
                        const sidebarButtons = document.querySelectorAll('.sidebar-content button');
                        const fieldset = document.querySelector('fieldset');
                        const textarea = document.querySelector('textarea.composer-input');
                        const ready = sidebarButtons.length >= 2 && fieldset && !fieldset.disabled && textarea && !textarea.disabled;
                        return ready ? true : null;
                    })()"#,
                )
                .await?;
            Ok(state.as_bool())
        },
    )
    .await
    {
        let diag = cdp
            .eval_json(
                &session_id,
                r#"(() => ({
                    status: document.querySelector('p[role="status"]')?.textContent?.trim() ?? '',
                    sidebar: Array.from(document.querySelectorAll('.sidebar-content button')).map(b => b.textContent.trim()),
                    newSessionDisabled: Array.from(document.querySelectorAll('button')).find(b => b.textContent.trim() === 'New session')?.disabled ?? null,
                    fieldsetDisabled: document.querySelector('fieldset')?.disabled ?? null,
                    textareaDisabled: document.querySelector('textarea.composer-input')?.disabled ?? null
                }))()"#,
            )
            .await
            .ok();
        let (exc, cons, _) = cdp.snapshot_telemetry();
        let server_log = std::fs::read_to_string(data_dir.path().join("server.logs.json")).unwrap_or_default();
        return Err(format!("{err}; diag={diag:?}; exc={exc:?}; cons={cons:?}; server_log={server_log}").into());
    }

    // 5. Type message into composer <textarea> and submit; verify user + Echo reply from p2 --worker.
    let submitted = cdp
        .eval_json(
            &session_id,
            r#"(() => {
                const textarea = document.querySelector('textarea.composer-input');
                const send = document.querySelector('button.composer-send');
                if (!textarea || !send) return false;
                textarea.focus();
                textarea.value = 'hello native chromium e2e';
                textarea.dispatchEvent(new Event('input', { bubbles: true }));
                send.click();
                return true;
            })()"#,
        )
        .await?;
    assert_eq!(submitted, json!(true), "composer submit must succeed");

    if let Err(err) = wait_until(
        "user + Echo assistant message",
        Duration::from_secs(20),
        || async {
            let rows = cdp
                .eval_json(
                    &session_id,
                    r#"(() => ({
                        user: Array.from(document.querySelectorAll('[data-testid="chat"] [data-role="user"]')).map(e => e.innerText).join('\n'),
                        assistant: Array.from(document.querySelectorAll('[data-testid="chat"] [data-role="assistant"]')).map(e => e.innerText).join('\n'),
                    }))()"#,
                )
                .await?;
            let user = rows["user"].as_str().unwrap_or_default();
            let assistant = rows["assistant"].as_str().unwrap_or_default();
            Ok((user.contains("hello native chromium e2e")
                && assistant.contains("Echo:")
                && assistant.contains("hello native chromium e2e"))
            .then_some(rows))
        },
    )
    .await
    {
        let diag = cdp
            .eval_json(
                &session_id,
                r#"(() => ({
                    status: document.querySelector('p[role="status"]')?.textContent?.trim() ?? '',
                    chat: document.querySelector('[data-testid="chat"]')?.innerText ?? '',
                    draft: document.querySelector('textarea.composer-input')?.value ?? '',
                    fieldsetDisabled: document.querySelector('fieldset')?.disabled ?? null,
                    sidebar: Array.from(document.querySelectorAll('.sidebar-content button')).map(b => b.textContent.trim()),
                }))()"#,
            )
            .await
            .ok();
        let (exc, cons, _) = cdp.snapshot_telemetry();
        let server_log =
            std::fs::read_to_string(data_dir.path().join("server.logs.json")).unwrap_or_default();
        return Err(
            format!("{err}; diag={diag:?}; exc={exc:?}; cons={cons:?}; server_log={server_log}")
                .into(),
        );
    }
    assert_no_errors_and_assets_ok(&cdp, &session_id).await?;

    // 6. Reload via CDP Page.reload and verify reconnection + SQLite persistence of Echo reply.
    cdp.eval_json(&session_id, "window.__beforeReload = true")
        .await?;
    cdp.clear_telemetry();
    cdp.call(
        "Page.reload",
        json!({ "ignoreCache": false }),
        Some(&session_id),
    )
    .await?;
    wait_until("reloaded document", Duration::from_secs(10), || async {
        let reloaded = cdp.eval_json(&session_id, "!window.__beforeReload").await?;
        Ok((reloaded.as_bool() == Some(true)).then_some(()))
    })
    .await?;
    wait_for_connected(&cdp, &session_id).await?;
    if let Err(err) = wait_until(
        "reloaded Echo assistant message",
        Duration::from_secs(15),
        || async {
            let rows = cdp
                .eval_json(
                    &session_id,
                    r#"(() => ({
                        user: Array.from(document.querySelectorAll('[data-testid="chat"] [data-role="user"]')).map(e => e.innerText).join('\n'),
                        assistant: Array.from(document.querySelectorAll('[data-testid="chat"] [data-role="assistant"]')).map(e => e.innerText).join('\n'),
                    }))()"#,
                )
                .await?;
            let assistant = rows["assistant"].as_str().unwrap_or_default();
            Ok((assistant.contains("Echo:") && assistant.contains("hello native chromium e2e"))
                .then_some(rows))
        },
    )
    .await
    {
        let diag = cdp
            .eval_json(
                &session_id,
                r#"(() => ({
                    status: document.querySelector('p[role="status"]')?.textContent?.trim() ?? '',
                    sidebar: Array.from(document.querySelectorAll('.sidebar-content button')).map(b => b.textContent.trim()),
                    chat: document.querySelector('[data-testid="chat"]')?.innerText ?? ''
                }))()"#,
            )
            .await
            .ok();
        let (exc, cons, resp) = cdp.snapshot_telemetry();
        return Err(
            format!("{err}; diag={diag:?}; exc={exc:?}; cons={cons:?}; resp={resp:?}").into(),
        );
    }
    assert_no_errors_and_assets_ok(&cdp, &session_id).await?;

    // 7. Exercise live built-in plugin UI panels in Chromium (Models, Settings, Cost, Tree).
    for (btn_testid, panel_selector) in [
        ("panel-btn-models", r#"[data-testid="panel-models"]"#),
        ("panel-btn-settings", r#"[data-testid="panel-settings"]"#),
        ("panel-btn-cost", ".cost-sheet"),
        ("panel-btn-tree", r#"[data-testid="panel-tree"]"#),
    ] {
        let clicked = cdp
            .eval_json(
                &session_id,
                &format!(
                    r#"(() => {{
                        const btn = document.querySelector('[data-testid="{btn_testid}"]');
                        if (!btn) return false;
                        btn.click();
                        return true;
                    }})()"#
                ),
            )
            .await?;
        assert_eq!(clicked, json!(true), "button {btn_testid} must exist");
        wait_until(panel_selector, Duration::from_secs(10), || async {
            let panel = cdp
                .eval_json(
                    &session_id,
                    &format!(r#"document.querySelector('{panel_selector}')?.innerText ?? null"#),
                )
                .await?;
            Ok(panel.as_str().filter(|s| !s.is_empty()).map(str::to_owned))
        })
        .await?;
    }
    cdp.eval_json(
        &session_id,
        r#"document.querySelector('[data-testid="panel-btn-models"]')?.click()"#,
    )
    .await?;
    let models_str = wait_until("Models panel content", Duration::from_secs(10), || async {
        let text = cdp
            .eval_json(
                &session_id,
                r#"document.querySelector('[data-testid="panel-models"]')?.innerText ?? ''"#,
            )
            .await?;
        let s = text.as_str().unwrap_or_default();
        Ok((s.contains("faux") || s.contains("fake")).then_some(s.to_owned()))
    })
    .await?;
    assert!(
        models_str.contains("faux") || models_str.contains("fake"),
        "Models panel must display real server models: {models_str}"
    );
    assert_no_errors_and_assets_ok(&cdp, &session_id).await?;

    // 7. Clean teardown: close Chromium and SIGTERM p2 server.
    drop(cdp);
    chromium.close();
    p2_guard.terminate_and_wait()?;
    Ok(())
}

async fn connect_browser_chord(
    address: std::net::SocketAddr,
    cookie_header: &str,
) -> TestResult<(
    pi_rs_chord::remote::RemotePeer,
    Arc<dyn pi_rs_chord::link::Link>,
)> {
    use pi_rs_chord::{
        node::http::connect_websocket,
        remote::{RemoteOptions, RemotePeer},
        state::Batch,
    };
    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::COOKIE, cookie_header.parse()?);
    let link = connect_websocket(
        &format!("ws://{address}/chord?client=script"),
        headers,
        1024 * 1024,
        4 * 1024 * 1024,
    )
    .await?;
    let peer = RemotePeer::connect(
        link.clone(),
        RemoteOptions::new(json!({ "kind": "server" })),
        Batch::new(|_| {}),
    )?;
    Ok((peer, link))
}

async fn install_csp_tracker(cdp: &CdpClient, session_id: &str) -> TestResult {
    cdp.call(
        "Page.addScriptToEvaluateOnNewDocument",
        json!({
            "source": r#"
                window.__cspViolations = [];
                document.addEventListener('securitypolicyviolation', (e) => {
                    window.__cspViolations.push({
                        directive: e.violatedDirective,
                        blockedURI: e.blockedURI
                    });
                });
            "#
        }),
        Some(session_id),
    )
    .await?;
    Ok(())
}

async fn run_guest_share_expiration_e2e(p2_bin: &Path) -> TestResult {
    use pi_rs_chord::{context::Context, json::JsonValue};
    use pi_rs_p2::api::v1::p2_contracts::lookup;

    let data_dir = tempfile::tempdir()?;
    let owner_profile = tempfile::tempdir()?;
    let guest_profile = tempfile::tempdir()?;

    let mut p2_proc = spawn_p2_server(p2_bin, data_dir.path())?;
    let mut p2_guard = ChildGuard::new(&mut p2_proc.child);
    let origin = format!("http://{}", p2_proc.address);
    let port = p2_proc.address.port();

    let owner_chromium = ChromiumProcess::spawn(owner_profile.path())?;
    let owner_cdp = CdpClient::connect(&owner_chromium.ws_url).await?;
    let owner_sid = owner_cdp.attach_page().await?;
    owner_cdp.enable_page_domains(&owner_sid).await?;

    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?;
    let (cookie_name, cookie_value, _) =
        claim_local_owner_cookie(&http, &origin, port, data_dir.path()).await?;
    let owner_cookie = format!("{cookie_name}={cookie_value}");

    install_csp_tracker(&owner_cdp, &owner_sid).await?;
    owner_cdp
        .call(
            "Network.setCookie",
            json!({
                "name": cookie_name,
                "value": cookie_value,
                "url": origin,
                "path": "/",
                "httpOnly": true,
                "sameSite": "Lax"
            }),
            Some(&owner_sid),
        )
        .await?;
    owner_cdp.navigate(&owner_sid, &origin).await?;
    wait_for_connected(&owner_cdp, &owner_sid).await?;
    assert_no_errors_and_assets_ok(&owner_cdp, &owner_sid).await?;

    // Owner creates Session 1 and sends a message before sharing.
    let clicked = owner_cdp
        .eval_json(
            &owner_sid,
            r#"(() => {
                const btn = Array.from(document.querySelectorAll('button'))
                    .find((b) => b.textContent.trim() === 'New session' && !b.disabled);
                if (!btn) return false;
                btn.click();
                return true;
            })()"#,
        )
        .await?;
    assert_eq!(clicked, json!(true), "New session button must be clickable");

    wait_until("owner session ready", Duration::from_secs(15), || async {
        let ready = owner_cdp
            .eval_json(
                &owner_sid,
                r#"(() => {
                    const sidebarButtons = document.querySelectorAll('.sidebar-content button');
                    const fieldset = document.querySelector('fieldset');
                    const textarea = document.querySelector('textarea.composer-input');
                    return (sidebarButtons.length >= 2 && fieldset && !fieldset.disabled && textarea && !textarea.disabled) ? true : null;
                })()"#,
            )
            .await?;
        Ok(ready.as_bool())
    })
    .await?;

    owner_cdp
        .eval_json(
            &owner_sid,
            r#"(() => {
                const textarea = document.querySelector('textarea.composer-input');
                const send = document.querySelector('button.composer-send');
                textarea.focus();
                textarea.value = 'owner secret before share';
                textarea.dispatchEvent(new Event('input', { bubbles: true }));
                send.click();
                return true;
            })()"#,
        )
        .await?;
    wait_until(
        "owner echo before share",
        Duration::from_secs(20),
        || async {
            let text = owner_cdp
                .eval_json(
                    &owner_sid,
                    r#"document.querySelector('[data-testid="chat"]')?.innerText ?? ''"#,
                )
                .await?;
            let s = text.as_str().unwrap_or_default();
            Ok((s.contains("owner secret before share") && s.contains("Echo:")).then_some(true))
        },
    )
    .await?;

    // Use owner Chord peer to discover Session 1 ID, create unshared Session 2, and mint a view share link.
    let ctx = Context::background();
    let (owner_peer, _owner_link) = connect_browser_chord(p2_proc.address, &owner_cookie).await?;
    let sessions_contract = Arc::new(lookup("p2.sessions")?);
    let sessions_held =
        owner_peer.hold(sessions_contract, JsonValue::from_serde(json!({})), &ctx)?;
    tokio::time::timeout(Duration::from_secs(5), sessions_held.wait(&ctx)).await??;
    let sessions_val = sessions_held
        .member("sessions")?
        .value()?
        .to_json()
        .to_serde()?;
    let session1_id = sessions_val
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|row| row["id"].as_str())
        .ok_or("missing owner session1 id")?
        .to_owned();
    let default_cwd = sessions_held
        .member("defaultCwd")?
        .value()?
        .to_json()
        .to_serde()?;
    let session2_val = sessions_held
        .call(
            "create",
            JsonValue::from_serde(json!({ "cwd": default_cwd })),
            &ctx,
        )
        .await?
        .to_serde()?;
    let session2_id = session2_val["id"]
        .as_str()
        .ok_or("missing session2 id")?
        .to_owned();

    let sharing_contract = Arc::new(lookup("p2.sharing")?);
    let sharing_held = owner_peer.hold(
        sharing_contract,
        JsonValue::from_serde(json!({ "session": session1_id })),
        &ctx,
    )?;
    tokio::time::timeout(Duration::from_secs(5), sharing_held.wait(&ctx)).await??;
    sharing_held
        .call(
            "link",
            JsonValue::from_serde(json!({ "mode": "view", "reset": false })),
            &ctx,
        )
        .await?;
    let share_token = wait_until("view share link token", Duration::from_secs(5), || async {
        owner_peer.fence().await?;
        let links = sharing_held
            .member("links")?
            .value()?
            .to_json()
            .to_serde()?;
        let tok = links
            .as_array()
            .and_then(|arr| arr.iter().find(|l| l["mode"] == "view"))
            .and_then(|l| l["token"].as_str())
            .map(str::to_owned);
        Ok(tok)
    })
    .await?;

    // Guest Chromium joins via /join/{token} under production CSP.
    let guest_chromium = ChromiumProcess::spawn(guest_profile.path())?;
    let guest_cdp = CdpClient::connect(&guest_chromium.ws_url).await?;
    let guest_sid = guest_cdp.attach_page().await?;
    guest_cdp.enable_page_domains(&guest_sid).await?;
    install_csp_tracker(&guest_cdp, &guest_sid).await?;

    guest_cdp
        .navigate(&guest_sid, &format!("{origin}/join/{share_token}"))
        .await?;
    wait_until("guest join form", Duration::from_secs(10), || async {
        let ready = guest_cdp
            .eval_json(
                &guest_sid,
                r#"Boolean(document.getElementById('join') && document.getElementById('name'))"#,
            )
            .await?;
        Ok((ready.as_bool() == Some(true)).then_some(()))
    })
    .await?;
    guest_cdp
        .eval_json(
            &guest_sid,
            r#"(() => {
                const input = document.getElementById('name');
                const form = document.getElementById('join');
                input.value = 'Guest Viewer';
                form.requestSubmit();
                return true;
            })()"#,
        )
        .await?;

    wait_until(
        "guest redirected to shared session",
        Duration::from_secs(15),
        || async {
            let href = guest_cdp.eval_json(&guest_sid, "location.pathname").await?;
            Ok(href
                .as_str()
                .is_some_and(|p| p == format!("/s/{session1_id}"))
                .then_some(()))
        },
    )
    .await?;
    wait_for_connected(&guest_cdp, &guest_sid).await?;
    wait_until(
        "guest sees shared transcript",
        Duration::from_secs(15),
        || async {
            let text = guest_cdp
                .eval_json(
                    &guest_sid,
                    r#"document.querySelector('[data-testid="chat"]')?.innerText ?? ''"#,
                )
                .await?;
            let s = text.as_str().unwrap_or_default();
            Ok((s.contains("owner secret before share") && s.contains("Echo:")).then_some(true))
        },
    )
    .await?;
    let guest_readonly = guest_cdp
        .eval_json(
            &guest_sid,
            r#"(() => ({
                fieldsetDisabled: document.querySelector('fieldset')?.disabled ?? false,
                newBtnDisabled: Array.from(document.querySelectorAll('button')).find(b => b.textContent.trim() === 'New session')?.disabled ?? false
            }))()"#,
        )
        .await?;
    assert_eq!(guest_readonly["fieldsetDisabled"], json!(true));
    assert_eq!(guest_readonly["newBtnDisabled"], json!(true));
    assert_no_errors_and_assets_ok(&guest_cdp, &guest_sid).await?;

    // Verify guest cannot access unshared Session 2 via HTTP or Chord.
    let cookies_resp = guest_cdp
        .call(
            "Network.getCookies",
            json!({ "urls": [origin] }),
            Some(&guest_sid),
        )
        .await?;
    let guest_cookie_name = format!("p2_guest_{port}");
    let guest_cookie_val = cookies_resp["cookies"]
        .as_array()
        .and_then(|arr| arr.iter().find(|c| c["name"] == guest_cookie_name))
        .and_then(|c| c["value"].as_str())
        .ok_or("missing p2_guest cookie in Chromium")?
        .to_owned();
    let guest_cookie = format!("{guest_cookie_name}={guest_cookie_val}");

    let unshared_http = http
        .get(format!("{origin}/s/{session2_id}"))
        .header(reqwest::header::COOKIE, &guest_cookie)
        .send()
        .await?;
    assert_eq!(unshared_http.status(), reqwest::StatusCode::FORBIDDEN);

    let (guest_peer, _guest_link) = connect_browser_chord(p2_proc.address, &guest_cookie).await?;
    let chat_contract = Arc::new(lookup("p2.chat")?);
    let unshared_hold = guest_peer.hold(
        chat_contract,
        JsonValue::from_serde(json!({ "session": session2_id, "conversation": 1 })),
        &ctx,
    )?;
    let unshared_res =
        tokio::time::timeout(Duration::from_secs(5), unshared_hold.wait(&ctx)).await?;
    assert!(
        matches!(unshared_res, Err(pi_rs_chord::Error::Chord(ref e)) if e.code == "forbidden"),
        "guest hold on unshared session must be forbidden: {unshared_res:?}"
    );
    guest_peer.close();

    // Expire / revoke the guest share by resetting the view link.
    sharing_held
        .call(
            "link",
            JsonValue::from_serde(json!({ "mode": "view", "reset": true })),
            &ctx,
        )
        .await?;

    // Guest Chromium must transition to "Share ended" and clear session/transcript state.
    wait_until(
        "guest sees Share ended",
        Duration::from_secs(15),
        || async {
            let state = guest_cdp
                .eval_json(
                    &guest_sid,
                    r#"(() => ({
                    status: document.querySelector('p[role="status"]')?.textContent?.trim() ?? '',
                    rows: document.querySelectorAll('[data-testid="chat"] .chat-row').length
                }))()"#,
                )
                .await?;
            let ok = state["status"].as_str() == Some("Share ended")
                && state["rows"].as_u64() == Some(0);
            Ok(ok.then_some(()))
        },
    )
    .await?;

    // Old join link returns 410 Gone; old guest cookie fails HTTP (401) and WebSocket upgrade.
    let expired_join = http
        .get(format!("{origin}/join/{share_token}"))
        .send()
        .await?;
    assert_eq!(expired_join.status(), reqwest::StatusCode::GONE);
    let expired_session_http = http
        .get(format!("{origin}/s/{session1_id}"))
        .header(reqwest::header::COOKIE, &guest_cookie)
        .send()
        .await?;
    assert_eq!(
        expired_session_http.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let revoked_ws = connect_browser_chord(p2_proc.address, &guest_cookie).await;
    assert!(
        revoked_ws.is_err(),
        "revoked guest WebSocket must be rejected"
    );

    // Reloading Guest Chromium lands on Sign-in page under CSP.
    guest_cdp.clear_telemetry();
    guest_cdp
        .navigate(&guest_sid, &format!("{origin}/s/{session1_id}"))
        .await?;
    wait_until(
        "guest lands on sign-in page",
        Duration::from_secs(10),
        || async {
            let ready = guest_cdp
                .eval_json(
                    &guest_sid,
                    r#"(() => ({
                    hasForm: Boolean(document.getElementById('sign-in')),
                    hasRoot: Boolean(document.getElementById('root')),
                    text: document.body ? document.body.innerText : ''
                }))()"#,
                )
                .await?;
            let ok = ready["hasForm"].as_bool() == Some(true)
                && ready["hasRoot"].as_bool() == Some(false)
                && ready["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("Sign in to Pi"));
            Ok(ok.then_some(()))
        },
    )
    .await?;
    let guest_csp = guest_cdp
        .eval_json(&guest_sid, "window.__cspViolations || []")
        .await?;
    assert_eq!(guest_csp, json!([]));

    // Owner remains connected and unaffected.
    owner_cdp.clear_telemetry();
    owner_cdp
        .eval_json(
            &owner_sid,
            r#"(() => {
                const textarea = document.querySelector('textarea.composer-input');
                const send = document.querySelector('button.composer-send');
                if (!textarea || !send || textarea.disabled) return false;
                textarea.focus();
                textarea.value = 'owner after share expired';
                textarea.dispatchEvent(new Event('input', { bubbles: true }));
                send.click();
                return true;
            })()"#,
        )
        .await?;
    wait_until(
        "owner echo after share expired",
        Duration::from_secs(20),
        || async {
            let text = owner_cdp
                .eval_json(
                    &owner_sid,
                    r#"document.querySelector('[data-testid="chat"]')?.innerText ?? ''"#,
                )
                .await?;
            let s = text.as_str().unwrap_or_default();
            Ok((s.contains("owner after share expired") && s.contains("Echo:")).then_some(true))
        },
    )
    .await?;

    owner_peer.close();
    drop(guest_cdp);
    drop(owner_cdp);
    guest_chromium.close();
    owner_chromium.close();
    p2_guard.terminate_and_wait()?;
    Ok(())
}

async fn wait_for_connected(cdp: &CdpClient, session_id: &str) -> TestResult {
    let res = wait_until("Connected status", Duration::from_secs(20), || async {
        let state = cdp
            .eval_json(
                session_id,
                r#"(() => {
                    const status = document.querySelector('p[role="status"]')?.textContent?.trim() ?? '';
                    const newBtn = Array.from(document.querySelectorAll('button'))
                        .find((b) => b.textContent.trim() === 'New session');
                    const ready = status === 'Connected' || (status === 'Loading' && newBtn && !newBtn.disabled);
                    return ready ? status : null;
                })()"#,
            )
            .await?;
        Ok(state.as_str().map(str::to_owned))
    })
    .await;
    if let Err(err) = res {
        let diag = cdp
            .eval_json(
                session_id,
                r#"(() => ({
                    status: document.querySelector('p[role="status"]')?.textContent?.trim() ?? '',
                    newSessionDisabled: Array.from(document.querySelectorAll('button')).find(b => b.textContent.trim() === 'New session')?.disabled ?? null
                }))()"#,
            )
            .await
            .ok();
        let (exc, cons, resp) = cdp.snapshot_telemetry();
        return Err(format!(
            "{err}; diag={diag:?}; exceptions={exc:?}; console_errors={cons:?}; responses={resp:?}"
        )
        .into());
    }
    Ok(())
}

async fn assert_no_errors_and_assets_ok(cdp: &CdpClient, session_id: &str) -> TestResult {
    let _ = cdp
        .eval_json(session_id, "document.fonts.ready.then(() => true)")
        .await?;
    let csp_violations = cdp
        .eval_json(session_id, "window.__cspViolations || []")
        .await?;
    assert_eq!(
        csp_violations,
        json!([]),
        "unexpected CSP securitypolicyviolation events: {csp_violations}"
    );
    let (exceptions, console_errors, responses) = cdp.snapshot_telemetry();
    assert!(
        exceptions.is_empty(),
        "unexpected Runtime.exceptionThrown: {exceptions:?}"
    );
    assert!(
        console_errors.is_empty(),
        "unexpected console/log errors: {console_errors:?}"
    );
    for required in ["app.js", "app_bg.wasm", "p2.css"] {
        let found = responses.iter().find(|(url, _)| {
            url.contains("/_chord/web/") && url.ends_with(&format!("/{required}"))
        });
        let Some((url, status)) = found else {
            return Err(format!("missing network response for {required}: {responses:?}").into());
        };
        assert_eq!(*status, 200, "asset {url} returned status {status}");
    }
    for (url, status) in responses.iter().filter(|(url, _)| url.ends_with(".woff2")) {
        assert_eq!(*status, 200, "font {url} returned status {status}");
    }
    Ok(())
}

async fn claim_local_owner_cookie(
    client: &reqwest::Client,
    origin: &str,
    port: u16,
    data_dir: &Path,
) -> TestResult<(String, String, String)> {
    let key = std::fs::read_to_string(data_dir.join("owner-key"))?;
    let challenge_resp = client
        .post(format!("{origin}{LOCAL_CHALLENGE_PATH}"))
        .header(reqwest::header::ORIGIN, origin)
        .json(&json!({}))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    let challenge = challenge_resp["challenge"]
        .as_str()
        .ok_or("missing local owner challenge")?;
    let proof = local_owner_proof(key.trim(), port, challenge);
    let claim_resp = client
        .post(format!("{origin}{LOCAL_CLAIM_PATH}"))
        .header(reqwest::header::ORIGIN, origin)
        .json(&json!({ "challenge": challenge, "proof": proof }))
        .send()
        .await?
        .error_for_status()?;
    let raw_cookie = claim_resp
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or("missing Set-Cookie header from local owner claim")?
        .to_owned();
    let pair = raw_cookie
        .split(';')
        .next()
        .ok_or("empty Set-Cookie header")?;
    let (name, value) = pair
        .split_once('=')
        .ok_or("malformed Set-Cookie name=value")?;
    Ok((name.to_owned(), value.to_owned(), raw_cookie))
}

fn find_workspace_manifest(start: &Path) -> TestResult<PathBuf> {
    for dir in start.ancestors() {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file()
            && let Ok(text) = std::fs::read_to_string(&candidate)
            && text.contains("[workspace]")
        {
            return Ok(candidate);
        }
    }
    Err(format!(
        "cannot locate workspace Cargo.toml from {}",
        start.display()
    )
    .into())
}

fn resolve_packaged_p2_binary() -> TestResult<PathBuf> {
    if let Some(explicit) = std::env::var_os("P2_PACKAGED_BIN") {
        let path = PathBuf::from(explicit);
        if !path.is_file() {
            return Err(format!("P2_PACKAGED_BIN does not exist: {}", path.display()).into());
        }
        return Ok(path);
    }
    if pi_rs_p2::hosts::web::browser_base().is_some() {
        return Ok(PathBuf::from(env!("CARGO_BIN_EXE_p2")));
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let cargo_manifest = find_workspace_manifest(&manifest_dir)?;
    let workspace_root = cargo_manifest.parent().ok_or("missing workspace root")?;
    let cargo_manifest_str = cargo_manifest.to_str().ok_or("non-UTF-8 Cargo.toml path")?;

    let wasm_bindgen_ok = Command::new("wasm-bindgen")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !wasm_bindgen_ok {
        return Err("missing wasm-bindgen CLI".into());
    }

    let target_root = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"));
    let cache_dir = target_root.join("p2-packaged-e2e");
    let assets_dir = cache_dir.join("assets");
    std::fs::create_dir_all(&assets_dir)?;

    run_checked(
        Command::new("cargo")
            .args([
                "run",
                "--manifest-path",
                cargo_manifest_str,
                "-p",
                "pi-rs-p2-web",
                "--bin",
                "p2-bundle-assets",
                "--offline",
                "--locked",
                "--",
                "--out-dir",
                assets_dir.to_str().ok_or("non-UTF-8 assets dir")?,
            ])
            .env("CARGO_TARGET_DIR", &target_root),
        "bundle browser assets with p2-bundle-assets",
    )?;

    run_checked(
        Command::new("cargo")
            .args([
                "build",
                "--manifest-path",
                cargo_manifest_str,
                "-p",
                "pi-rs-p2-web",
                "--lib",
                "--target",
                "wasm32-unknown-unknown",
                "--offline",
                "--locked",
            ])
            .env("CARGO_TARGET_DIR", &target_root),
        "build pi-rs-p2-web wasm",
    )?;

    let wasm_path = target_root.join("wasm32-unknown-unknown/debug/pi_rs_p2_web.wasm");
    run_checked(
        Command::new("wasm-bindgen").args([
            "--target",
            "web",
            "--no-typescript",
            "--out-name",
            "app",
            "--out-dir",
            assets_dir.to_str().ok_or("non-UTF-8 assets dir")?,
            wasm_path.to_str().ok_or("non-UTF-8 wasm path")?,
        ]),
        "run wasm-bindgen",
    )?;

    run_checked(
        Command::new("cargo")
            .args([
                "build",
                "--manifest-path",
                cargo_manifest_str,
                "-p",
                "pi-rs-p2",
                "--bin",
                "p2",
                "--offline",
                "--locked",
            ])
            .env("CARGO_TARGET_DIR", &target_root)
            .env("P2_BROWSER_ASSETS", &assets_dir),
        "build packaged p2 binary",
    )?;

    Ok(target_root.join("debug/p2"))
}

fn run_checked(cmd: &mut Command, label: &str) -> TestResult {
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(format!(
            "{label} failed (status {}):\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(())
}

struct P2Process {
    child: Child,
    address: std::net::SocketAddr,
}

fn spawn_p2_server(bin: &Path, data_dir: &Path) -> TestResult<P2Process> {
    let mut child = Command::new(bin)
        .env("PORT", "0")
        .env("P2_FAKE", "1")
        .env("P2_DATA_DIR", data_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let stdout = child.stdout.take().ok_or("missing p2 stdout")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(rest) = line.strip_prefix("p2: http://")
                && let Some(addr_str) = rest.split_whitespace().next()
                && let Ok(addr) = addr_str.parse::<std::net::SocketAddr>()
            {
                let _ = tx.send(addr);
                break;
            }
        }
    });
    let address = rx
        .recv_timeout(Duration::from_secs(30))
        .map_err(|_| "timed out waiting for p2 listening address on stdout")?;
    Ok(P2Process { child, address })
}

struct ChildGuard<'a> {
    child: &'a mut Child,
    done: bool,
}

impl<'a> ChildGuard<'a> {
    fn new(child: &'a mut Child) -> Self {
        Self { child, done: false }
    }

    fn terminate_and_wait(&mut self) -> TestResult {
        self.done = true;
        #[cfg(unix)]
        {
            let pid = self.child.id().to_string();
            let _ = Command::new("kill").args(["-TERM", &pid]).status();
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait()? {
                if !status.success() {
                    return Err(format!("p2 exited with non-zero status: {status}").into());
                }
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err("timed out waiting for p2 clean shutdown on SIGTERM".into());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for ChildGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct ChromiumProcess {
    child: Child,
    ws_url: String,
}

impl ChromiumProcess {
    fn spawn(user_data_dir: &Path) -> TestResult<Self> {
        let mut child = Command::new(CHROMIUM_BIN)
            .args([
                "--headless=new",
                "--no-sandbox",
                "--disable-gpu",
                "--disable-dev-shm-usage",
                "--no-first-run",
                "--no-default-browser-check",
                "--remote-debugging-port=0",
            ])
            .arg(format!("--user-data-dir={}", user_data_dir.display()))
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;

        let stderr = child.stderr.take().ok_or("missing Chromium stderr")?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                if let Some(ws) = line.strip_prefix("DevTools listening on ") {
                    let _ = tx.send(ws.trim().to_owned());
                    break;
                }
            }
        });
        let ws_url = rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| "timed out waiting for Chromium DevTools WebSocket URL")?;
        Ok(Self { child, ws_url })
    }

    fn close(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ChromiumProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct Telemetry {
    exceptions: Vec<String>,
    console_errors: Vec<String>,
    responses: Vec<(String, u16)>,
}

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct CdpClient {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    pending: PendingMap,
    telemetry: Arc<Mutex<Telemetry>>,
    next_id: AtomicU64,
    reader_task: tokio::task::JoinHandle<()>,
    writer_task: tokio::task::JoinHandle<()>,
}

impl CdpClient {
    async fn connect(ws_url: &str) -> TestResult<Self> {
        let (stream, _) = connect_async(ws_url).await?;
        let (mut write, mut read) = stream.split();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let telemetry = Arc::new(Mutex::new(Telemetry::default()));

        let writer_task = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if write.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
        });

        let pending_reader = Arc::clone(&pending);
        let telemetry_reader = Arc::clone(&telemetry);
        let reader_task = tokio::spawn(async move {
            while let Some(Ok(msg)) = read.next().await {
                let Message::Text(text) = msg else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(id) = value.get("id").and_then(Value::as_u64) {
                    if let Some(sender) = pending_reader
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id)
                    {
                        if let Some(err) = value.get("error") {
                            let _ = sender.send(Err(err.to_string()));
                        } else {
                            let _ = sender
                                .send(Ok(value.get("result").cloned().unwrap_or(Value::Null)));
                        }
                    }
                    continue;
                }
                let Some(method) = value.get("method").and_then(Value::as_str) else {
                    continue;
                };
                let params = value.get("params").unwrap_or(&Value::Null);
                let mut t = telemetry_reader.lock().unwrap_or_else(|e| e.into_inner());
                match method {
                    "Runtime.exceptionThrown" => {
                        t.exceptions.push(params.to_string());
                    }
                    "Runtime.consoleAPICalled" => {
                        if params.get("type").and_then(Value::as_str) == Some("error") {
                            t.console_errors.push(params.to_string());
                        }
                    }
                    "Log.entryAdded" => {
                        let entry = &params["entry"];
                        let level = entry["level"].as_str().unwrap_or_default();
                        let source = entry["source"].as_str().unwrap_or_default();
                        if level == "error" || source == "security" {
                            t.console_errors.push(entry.to_string());
                        }
                    }
                    "Network.responseReceived" => {
                        let resp = &params["response"];
                        if let (Some(url), Some(status)) =
                            (resp["url"].as_str(), resp["status"].as_u64())
                        {
                            t.responses.push((url.to_owned(), status as u16));
                        }
                    }
                    _ => {}
                }
            }
        });

        Ok(Self {
            tx,
            pending,
            telemetry,
            next_id: AtomicU64::new(1),
            reader_task,
            writer_task,
        })
    }

    async fn call(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> TestResult<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        let mut payload = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(sid) = session_id {
            payload["sessionId"] = Value::String(sid.to_owned());
        }
        self.tx
            .send(payload.to_string())
            .map_err(|_| "CDP connection closed")?;
        let res = tokio::time::timeout(Duration::from_secs(15), rx)
            .await
            .map_err(|_| format!("CDP timeout on {method}"))?
            .map_err(|_| format!("CDP dropped response on {method}"))?;
        res.map_err(|e| format!("CDP error on {method}: {e}").into())
    }

    async fn attach_page(&self) -> TestResult<String> {
        let targets = self.call("Target.getTargets", json!({}), None).await?;
        let target_id = targets["targetInfos"].as_array().and_then(|arr| {
            arr.iter()
                .find(|t| t["type"].as_str() == Some("page"))
                .and_then(|t| t["targetId"].as_str())
                .map(str::to_owned)
        });
        let target_id = match target_id {
            Some(id) => id,
            None => self
                .call("Target.createTarget", json!({ "url": "about:blank" }), None)
                .await?["targetId"]
                .as_str()
                .ok_or("missing created targetId")?
                .to_owned(),
        };
        let attached = self
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                None,
            )
            .await?;
        Ok(attached["sessionId"]
            .as_str()
            .ok_or("missing CDP sessionId")?
            .to_owned())
    }

    async fn enable_page_domains(&self, session_id: &str) -> TestResult {
        for domain in [
            "Page.enable",
            "Runtime.enable",
            "Network.enable",
            "Log.enable",
        ] {
            self.call(domain, json!({}), Some(session_id)).await?;
        }
        Ok(())
    }

    async fn navigate(&self, session_id: &str, url: &str) -> TestResult {
        let nav = self
            .call("Page.navigate", json!({ "url": url }), Some(session_id))
            .await?;
        if let Some(err) = nav.get("errorText").and_then(Value::as_str) {
            return Err(format!("Page.navigate to {url} failed: {err}").into());
        }
        Ok(())
    }

    async fn eval_json(&self, session_id: &str, expression: &str) -> TestResult<Value> {
        let res = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "awaitPromise": true,
                    "returnByValue": true
                }),
                Some(session_id),
            )
            .await?;
        if let Some(exc) = res.get("exceptionDetails") {
            return Err(format!("Runtime.evaluate exception: {exc}").into());
        }
        Ok(res["result"].get("value").cloned().unwrap_or(Value::Null))
    }

    fn clear_telemetry(&self) {
        let mut t = self.telemetry.lock().unwrap_or_else(|e| e.into_inner());
        t.exceptions.clear();
        t.console_errors.clear();
        t.responses.clear();
    }

    fn snapshot_telemetry(&self) -> (Vec<String>, Vec<String>, Vec<(String, u16)>) {
        let t = self.telemetry.lock().unwrap_or_else(|e| e.into_inner());
        (
            t.exceptions.clone(),
            t.console_errors.clone(),
            t.responses.clone(),
        )
    }
}

impl Drop for CdpClient {
    fn drop(&mut self) {
        self.reader_task.abort();
        self.writer_task.abort();
    }
}

async fn wait_until<F, Fut, T>(label: &str, timeout: Duration, mut check: F) -> TestResult<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = TestResult<Option<T>>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(value) = check().await? {
            return Ok(value);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("timed out waiting for {label} after {timeout:?}").into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
