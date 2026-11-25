use pi_rs_chord::{context::Context, host::*, node::plugins::*};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn native_plugin_digest_reuse_and_failed_setup_hold_back_old_facet() {
    let host = create_host(HostOptions::default()).unwrap();
    let setups = Arc::new(AtomicUsize::new(0));
    let count = setups.clone();
    let facet = define_facet(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let script = PluginScript::native("native", b"v1", facet.clone());
    let digest = script.digest().to_string();
    let (runtime, report) =
        PluginRuntime::load(host.clone(), vec![], vec![script], &Context::background())
            .await
            .unwrap();
    assert_eq!(report.added, ["native"]);
    assert_eq!(setups.load(Ordering::SeqCst), 1);
    let replacement = define_facet(|_| {
        Err(pi_rs_chord::Error::Invalid(
            "must reuse unchanged digest".into(),
        ))
    });
    let unchanged = runtime
        .reload(
            vec![PluginScript::native("native", b"v1", replacement)],
            &Context::background(),
        )
        .await
        .unwrap();
    assert!(unchanged.changed.is_empty());
    assert!(unchanged.failed.is_empty());
    assert_eq!(setups.load(Ordering::SeqCst), 1);
    let failure = runtime
        .reload(
            vec![PluginScript::native(
                "native",
                b"v2",
                define_facet(|_| Err(pi_rs_chord::Error::Invalid("broken setup".into()))),
            )],
            &Context::background(),
        )
        .await
        .unwrap();
    assert_eq!(failure.failed[0].plugin, "native");
    assert_eq!(runtime.digest("native"), Some(digest));
    let removed = runtime
        .reload(vec![], &Context::background())
        .await
        .unwrap();
    assert_eq!(removed.removed, ["native"]);
    host.close(&Context::background()).await.unwrap();
}

#[test]
fn discovery_groups_wasm_kinds_reports_duplicates_and_required_roots() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("hello.server.wasm"), b"component").unwrap();
    std::fs::write(temp.path().join("hello.worker.wasm"), b"component").unwrap();
    let roots = vec![
        PluginRoot {
            path: temp.path().into(),
            required: true,
        },
        PluginRoot {
            path: temp.path().join("absent"),
            required: true,
        },
    ];
    let found = discover_plugins(&roots, &["server", "worker"]).unwrap();
    assert_eq!(found.plugins.len(), 1);
    assert_eq!(found.plugins[0].name, "hello");
    assert_eq!(found.plugins[0].facets.len(), 2);
    assert_eq!(found.problems.len(), 1);
    assert!(found.problems[0].required);
    let duplicate = discover_plugins(&[roots[0].clone(), roots[0].clone()], &["server"]).unwrap();
    assert_eq!(duplicate.plugins.len(), 1);
    assert_eq!(duplicate.problems.len(), 1);
}

#[tokio::test]
async fn component_engine_instantiates_offline_and_rejects_missing_plugin_exports() {
    let host = WasmPluginHost::new().unwrap();
    // Empty Component Model binary, not a core module. No imports or ambient capabilities.
    let bytes = b"\0asm\x0d\0\x01\0";
    host.instantiate_component(bytes).await.unwrap();
    assert!(host.prepare(bytes).await.is_err());
    assert!(host.instantiate_component(b"not wasm").await.is_err());
}

#[tokio::test]
async fn plugin_watcher_observes_real_file_changes_and_debounces() {
    let temp = tempfile::tempdir().unwrap();
    let mut watcher = PluginWatcher::new(
        &[temp.path().to_path_buf()],
        std::time::Duration::from_millis(50),
    )
    .unwrap();
    std::fs::write(temp.path().join("new.server.wasm"), b"v1").unwrap();
    let changed = tokio::time::timeout(std::time::Duration::from_secs(5), watcher.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(!changed.is_empty());
    assert!(changed.iter().any(|p| p.ends_with("new.server.wasm")));
    watcher.close();
}
