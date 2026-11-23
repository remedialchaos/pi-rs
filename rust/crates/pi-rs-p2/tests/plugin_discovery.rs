use pi_rs_chord::{
    Result,
    context::Context,
    contracts::{
        Contract, Member,
        schema::{int, method},
    },
    host::{HostOptions, create_host, define_facet},
    json::JsonValue,
};
use pi_rs_p2::{
    api::v1::{FacetKind, PluginRegistry},
    hosts::plugins::PluginSources,
};
use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[tokio::test]
async fn compiled_names_win_and_rejected_user_worker_keeps_serving() -> Result<()> {
    const V1: &[u8] = include_bytes!("../../pi-rs-chord/tests/fixtures/b3/v1.wasm");
    const REJECT: &[u8] = include_bytes!("../../pi-rs-chord/tests/fixtures/b3/reject.wasm");
    let dir = tempfile::tempdir()?;
    let mut registry = PluginRegistry::new();
    let activated = Arc::new(AtomicBool::default());
    let flag = activated.clone();
    registry.register(
        "native",
        FacetKind::Worker,
        define_facet(move |_| {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        }),
    )?;
    registry.register("excluded", FacetKind::Server, define_facet(|_| Ok(())))?;
    for name in ["native", "excluded"] {
        fs::write(dir.path().join(format!("{name}.worker.wasm")), b"invalid")?;
    }
    let package = dir.path().join("counter");
    fs::create_dir(&package)?;
    let manifest = package.join("package.json");
    let valid = br#"{"name":"counter"}"#;
    fs::write(&manifest, valid)?;
    let file = package.join("worker.wasm");
    fs::write(&file, V1)?;
    fs::write(package.join("server.wasm"), b"wrong kind")?;
    let mut sources = PluginSources::new(
        &registry,
        Some(&["native".into()]),
        dir.path(),
        FacetKind::Worker,
    )?;
    let ctx = Context::background();
    let host = create_host(HostOptions::default())?;
    let (runtime, update) = sources.load(host.clone(), vec![], &ctx).await?;
    assert_eq!(update.selected, ["counter", "native"]);
    assert_eq!(update.problems.len(), 2);
    assert!(update.report.failed.is_empty());
    assert!(activated.load(Ordering::SeqCst));
    let contract = Arc::new(Contract::local(
        "guest-counter",
        1,
        vec![],
        indexmap::IndexMap::from([
            ("total".into(), Member::State(int())),
            ("add".into(), method(Some(int()), Some(int()))),
        ]),
    )?);
    let held = host.hold(contract, JsonValue::Object(Default::default()), &ctx)?;
    tokio::time::timeout(std::time::Duration::from_secs(15), held.wait(&ctx))
        .await
        .map_err(|error| pi_rs_chord::Error::Invalid(error.to_string()))??;
    assert_eq!(
        held.call("add", JsonValue::Number(3.), &ctx).await?,
        JsonValue::Number(3.)
    );
    fs::write(&manifest, b"{")?;
    assert_eq!(sources.reload(&runtime, &ctx).await?.report.failed.len(), 1);
    assert_eq!(held.read("total")?, JsonValue::Number(3.));
    fs::write(&manifest, valid)?;
    fs::write(&file, REJECT)?;
    let update = sources.reload(&runtime, &ctx).await?;
    assert_eq!(update.report.failed.len(), 1);
    assert!(
        update.report.failed[0]
            .error
            .contains("fixture rejected setup")
    );
    assert_eq!(
        held.call("add", JsonValue::Number(2.), &ctx).await?,
        JsonValue::Number(5.)
    );
    fs::remove_file(file)?;
    assert_eq!(
        sources.reload(&runtime, &ctx).await?.report.removed,
        ["counter"]
    );
    host.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn server_reload_replaces_native_closure_and_child_serves_delivered_worker()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    use pi_rs_chord::node::workers::{WorkerOptions, Workers};
    use pi_rs_p2::hosts::{
        server::{ServerOptions, start_server},
        session_workers::demand_session,
    };
    use serde_json::json;
    use std::{path::Path, sync::atomic::AtomicUsize, time::Duration};
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("plugins");
    fs::create_dir(&root)?;
    fs::write(
        root.join("counter.server.wasm"),
        include_bytes!("../../pi-rs-chord/tests/fixtures/b3/v1.wasm"),
    )?;
    fs::write(
        root.join("counter.worker.wasm"),
        include_bytes!("../../pi-rs-chord/tests/fixtures/b3/double.wasm"),
    )?;
    let calls = Arc::new(AtomicUsize::new(0));
    let native = |value| -> Result<PluginRegistry> {
        let mut registry = PluginRegistry::new();
        let seen = calls.clone();
        registry.register(
            "native",
            FacetKind::Server,
            define_facet(move |_| {
                seen.store(value, Ordering::SeqCst);
                Ok(())
            }),
        )?;
        Ok(registry)
    };
    let mut options = ServerOptions::new(dir.path().to_owned());
    options.port = 0;
    options.dev = true;
    options.registry = native(1)?;
    let mut server = start_server(options).await?;
    assert_eq!(server.plugins, ["counter", "native"]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let ctx = Context::background();
    let contract = Arc::new(Contract::local(
        "guest-counter",
        1,
        vec![],
        indexmap::IndexMap::from([
            ("total".into(), Member::State(int())),
            ("add".into(), method(Some(int()), Some(int()))),
        ]),
    )?);
    let params = JsonValue::Object(Default::default());
    let local = server.host.hold(contract.clone(), params.clone(), &ctx)?;
    tokio::time::timeout(Duration::from_secs(10), local.wait(&ctx)).await??;
    assert_eq!(
        local.call("add", JsonValue::Number(2.), &ctx).await?,
        JsonValue::Number(2.)
    );
    fs::write(root.join("counter.server.wasm"), b"malformed replacement")?;
    let new_registry = native(2)?;
    let report = server.reload(&new_registry, None).await?;
    assert_eq!(report.changed, ["native"]);
    assert_eq!(report.failed[0].plugin, "counter");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        local.call("add", JsonValue::Number(1.), &ctx).await?,
        JsonValue::Number(3.)
    );
    use pi_rs_p2::hosts::dev::{DevCommand, channel};
    let command = DevCommand::Reload {
        plugins: Some(vec!["counter".into()]),
    };
    let refused = channel::send(dir.path(), &command).await?;
    assert!(!refused.ok);
    assert!(refused.message.contains("counter (server)"));
    fs::write(
        root.join("counter.server.wasm"),
        include_bytes!("../../pi-rs-chord/tests/fixtures/b3/double.wasm"),
    )?;
    let applied = channel::send(dir.path(), &command).await?;
    assert!(applied.ok, "{}", applied.message);
    assert_eq!(applied.plugins, Some(vec!["counter".to_owned()]));
    assert_eq!(
        local.call("add", JsonValue::Number(2.), &ctx).await?,
        JsonValue::Number(4.)
    );
    // This second valid component declares the same Chord type. Only a child which
    // registered the first component can reject the second for that conflict.
    fs::copy(
        root.join("counter.worker.wasm"),
        root.join("duplicate.worker.wasm"),
    )?;
    let diagnostics = dir.path().join("worker-stderr");
    let mut process = WorkerOptions::new(Path::new("sh").to_owned(), dir.path().join("locks"));
    process.args = vec![
        "-c".into(),
        r#"exec "$1" --worker 2>"$2""#.into(),
        "worker".into(),
        env!("CARGO_BIN_EXE_p2").into(),
        diagnostics.to_string_lossy().into_owned(),
    ];
    process.start_timeout = Duration::from_secs(30);
    let workers = Workers::new(process)?;
    let init = serde_json::from_value(
        json!({"app":"","dataDir":dir.path(),"fake":true,"telemetry":false,"generation":1,"values":{"session":{"id":"active","cwd":"/","model":{"provider":"faux","modelId":"model"},"thinkingLevel":"off"}}}),
    )?;
    let child = demand_session(
        &workers,
        init,
        &new_registry.select(None)?.into_iter().collect(),
    )
    .await?;
    child.wait_ready().await?;
    workers.close().await;
    let errors = fs::read_to_string(diagnostics)?;
    assert!(
        errors.contains("optional component duplicate was not registered"),
        "{errors}"
    );
    assert!(
        !errors.contains("optional component counter was not registered"),
        "{errors}"
    );
    server.close().await?;
    Ok(())
}
