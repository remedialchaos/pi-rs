use std::{error::Error, path::Path, time::Duration};

use pi_rs_chord::node::{http::HttpContribution, plugins::digest};
use pi_rs_p2::{
    api::v1::{FacetKind, PluginRegistry},
    hosts::{
        server::{P2Server, ServerOptions, start_server},
        tui::plugins::{PanelUpdate, Plugins, RemoteSource},
        tui_bundles::TuiManifest,
    },
};
use pi_rs_p2_tui::ThemeRef;
use pi_rs_tui::Component;
use reqwest::{Client, Url, header};
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn fixture_wasm_bytes() -> Result<Vec<u8>, std::io::Error> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../pi-rs-p2-tui/tests/fixtures/panel/notes.tui.wasm");
    std::fs::read(path)
}

fn install_test_plugin(
    data_dir: &Path,
    name: &str,
    wasm_bytes: &[u8],
) -> Result<(), std::io::Error> {
    let plugin_dir = data_dir.join("plugins").join(name);
    std::fs::create_dir_all(&plugin_dir)?;
    std::fs::write(
        plugin_dir.join("package.json"),
        serde_json::to_vec(&json!({
            "name": name,
            "chord": { "tui": "./tui.wasm" }
        }))?,
    )?;
    std::fs::write(plugin_dir.join("tui.wasm"), wasm_bytes)?;
    Ok(())
}

fn remove_test_plugin(data_dir: &Path, name: &str) -> Result<(), std::io::Error> {
    let plugin_dir = data_dir.join("plugins").join(name);
    if plugin_dir.exists() {
        std::fs::remove_dir_all(plugin_dir)?;
    }
    Ok(())
}

fn test_registry() -> Result<PluginRegistry, pi_rs_chord::Error> {
    let mut auth = HttpContribution::new("tui-remote-fixture");
    auth.authenticators.push(std::sync::Arc::new(|request| {
        Box::pin(async move {
            Ok(match request.header("cookie") {
                Some("fixture=owner") => Some(json!({"kind":"owner","access":"local"})),
                Some("fixture=guest") => Some(json!({
                    "kind":"guest","id":"guest","name":"Guest","home":"aa-11",
                    "sessions":{"aa-11":"view"},"revision":1
                })),
                Some("fixture=worker") => {
                    Some(json!({"kind":"worker","session":"aa-11","generation":1}))
                }
                _ => None,
            })
        })
    }));
    let mut registry = PluginRegistry::new();
    registry.register("tui-remote-fixture", FacetKind::Server, auth.facet())?;
    Ok(registry)
}

async fn start_test_server(data_dir: &Path) -> TestResult<P2Server> {
    let mut options = ServerOptions::new(data_dir.to_owned());
    options.port = 0;
    options.registry = test_registry()?;
    Ok(start_server(options).await?)
}

#[tokio::test]
async fn authenticated_tui_manifest_and_wasm_delivery() -> TestResult {
    let dir = tempfile::tempdir()?;
    let wasm = fixture_wasm_bytes()?;
    let expected_digest = digest(&wasm);
    install_test_plugin(dir.path(), "notes", &wasm)?;

    let server = start_test_server(dir.path()).await?;
    let origin = format!("http://{}", server.address);
    let base = Url::parse(&origin)?;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    // Unauthenticated request -> 401
    let unauth = client.get(base.join("/__p2_tui")?).send().await?;
    assert_eq!(unauth.status().as_u16(), 401);

    // Worker principal -> 403
    let worker = client
        .get(base.join("/__p2_tui")?)
        .header(header::ORIGIN, &origin)
        .header(header::COOKIE, "fixture=worker")
        .send()
        .await?;
    assert_eq!(worker.status().as_u16(), 403);

    // HEAD /__p2_tui with guest cookie -> 200 + Cache-Control: no-store
    let head_guest = client
        .head(base.join("/__p2_tui")?)
        .header(header::ORIGIN, &origin)
        .header(header::COOKIE, "fixture=guest")
        .send()
        .await?;
    assert_eq!(head_guest.status().as_u16(), 200);
    assert_eq!(
        head_guest
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );

    // GET /__p2_tui with owner cookie -> 200 JSON manifest
    let resp = client
        .get(base.join("/__p2_tui")?)
        .header(header::ORIGIN, &origin)
        .header(header::COOKIE, "fixture=owner")
        .send()
        .await?;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let manifest: TuiManifest = resp.json().await?;
    assert_eq!(manifest.plugins.len(), 1);
    assert_eq!(manifest.plugins[0].name, "notes");
    assert_eq!(manifest.plugins[0].digest, expected_digest);
    assert_eq!(
        manifest.plugins[0].url,
        format!("/__p2_tui/{expected_digest}.wasm")
    );
    assert_eq!(manifest.plugins[0].bytes, wasm.len());

    // Fetch WASM asset
    let asset_resp = client
        .get(base.join(&manifest.plugins[0].url)?)
        .header(header::ORIGIN, &origin)
        .header(header::COOKIE, "fixture=owner")
        .send()
        .await?;
    assert_eq!(asset_resp.status().as_u16(), 200);
    assert_eq!(
        asset_resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/wasm")
    );
    assert_eq!(
        asset_resp
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let body = asset_resp.bytes().await?;
    assert_eq!(digest(&body), expected_digest);
    assert_eq!(body.as_ref(), wasm.as_slice());

    // Unknown digest -> 404
    let unknown = format!("/__p2_tui/{}.wasm", "0".repeat(64));
    let missing = client
        .get(base.join(&unknown)?)
        .header(header::ORIGIN, &origin)
        .header(header::COOKIE, "fixture=owner")
        .send()
        .await?;
    assert_eq!(missing.status().as_u16(), 404);

    // Invalid asset path -> 404
    let bad = client
        .get(base.join("/__p2_tui/not-a-digest.wasm")?)
        .header(header::ORIGIN, &origin)
        .header(header::COOKIE, "fixture=owner")
        .send()
        .await?;
    assert_eq!(bad.status().as_u16(), 404);

    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn remote_plugins_fetch_execute_and_live_hot_swap_with_fault_isolation() -> TestResult {
    let dir = tempfile::tempdir()?;
    let wasm = fixture_wasm_bytes()?;
    install_test_plugin(dir.path(), "notes", &wasm)?;

    let mut server = start_test_server(dir.path()).await?;
    let origin = format!("http://{}", server.address);
    let base_url = Url::parse(&origin)?;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let remote = RemoteSource {
        client,
        base_url,
        origin,
        cookie: "fixture=owner".to_owned(),
        poll_interval: Duration::from_millis(60),
    };

    // No local fallback path: must load exclusively from server.
    let mut plugins = Plugins::load_remote_or_local(remote, None)
        .await?
        .ok_or("missing remote plugins")?;

    let id = plugins
        .slash("notes", &["chat", "owner"])
        .ok_or("missing /notes command")?
        .to_owned();
    plugins.run(&id, "hello").await?;
    let theme = ThemeRef::new(Default::default());
    let PanelUpdate::Show(panel) = plugins.panel(&theme)? else {
        return Err("expected panel after /notes hello".into());
    };
    let rows = panel.render(80);
    assert!(rows[0].contains("Guest notes hello"));

    // Replace with broken WASM bytes on server -> server reload -> client polls and rejects broken
    // facet while retaining last good version.
    install_test_plugin(dir.path(), "notes", b"not-valid-wasm-component")?;
    let registry = test_registry()?;
    server.reload(&registry, None).await?;

    let outcome = wait_for_poll_reload(&mut plugins).await?;
    assert!(
        !outcome.failed.is_empty(),
        "expected failed plugin report for broken wasm"
    );
    assert!(!outcome.active_removed);
    plugins.run(&id, "still-works").await?;
    let PanelUpdate::Show(panel) = plugins.panel(&theme)? else {
        return Err("expected retained good panel to keep working".into());
    };
    assert!(panel.render(80)[0].contains("Guest notes still-works"));

    // Remove plugin on server -> server reload -> client polls and removes command/active panel.
    remove_test_plugin(dir.path(), "notes")?;
    server.reload(&registry, None).await?;
    let outcome = wait_for_poll_reload(&mut plugins).await?;
    assert!(outcome.active_removed);
    assert!(plugins.slash("notes", &["chat", "owner"]).is_none());

    // Re-add valid plugin on server -> server reload -> client polls and restores it live.
    install_test_plugin(dir.path(), "notes", &wasm)?;
    server.reload(&registry, None).await?;
    let outcome = wait_for_poll_reload(&mut plugins).await?;
    assert!(outcome.failed.is_empty());
    let id = plugins
        .slash("notes", &["chat", "owner"])
        .ok_or("expected /notes to reappear after live reload")?
        .to_owned();
    plugins.run(&id, "back-online").await?;
    let PanelUpdate::Show(panel) = plugins.panel(&theme)? else {
        return Err("expected panel after re-adding plugin".into());
    };
    assert!(panel.render(80)[0].contains("Guest notes back-online"));

    plugins.close().await?;
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn remote_loader_rejects_digest_mismatch() -> TestResult {
    use axum::{Json, Router, routing::get};

    let wasm = fixture_wasm_bytes()?;
    let wrong_digest = "a".repeat(64);
    let manifest = json!({
        "version": "v1",
        "plugins": [{
            "name": "notes",
            "digest": wrong_digest,
            "url": format!("/__p2_tui/{wrong_digest}.wasm"),
            "bytes": wasm.len(),
        }]
    });
    let app = Router::new()
        .route("/__p2_tui", get(move || async move { Json(manifest) }))
        .route(
            &format!("/__p2_tui/{wrong_digest}.wasm"),
            get(move || async move { wasm }),
        );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let remote = RemoteSource {
        client: Client::new(),
        base_url: Url::parse(&origin)?,
        origin,
        cookie: "fixture=owner".to_owned(),
        poll_interval: Duration::from_secs(1),
    };
    let result = Plugins::load_remote_or_local(remote, None).await;
    server.abort();
    assert!(result.is_err(), "expected digest mismatch to be rejected");
    Ok(())
}

async fn wait_for_poll_reload(
    plugins: &mut Plugins,
) -> TestResult<pi_rs_p2::hosts::tui::plugins::ReloadOutcome> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(outcome) = plugins.poll_reload().await? {
            return Ok(outcome);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("timed out waiting for background remote plugin reload".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
