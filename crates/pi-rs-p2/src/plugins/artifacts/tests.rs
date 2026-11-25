use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use pi_rs_chord::{
    Error, Result,
    context::Context,
    data::ChordError,
    host::{Caller, Host, HostOptions, PluginFacet, create_host},
    json::JsonValue,
    sqlite::{Options as SqliteOptions, SqliteDatabase},
    store::StoreEngine,
};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::{
    api::v1::{
        FacetKind, OwnerAccess, PluginRegistry, Principal, p2_contracts::lookup, server_address,
    },
    hosts::server::{ServerOptions, start_server},
    plugins::artifacts::{
        frame::{FRAME_CSP, frame_page},
        pages::{ErrorMark, EvalRequest, EvalResult, PageRegistry},
        register,
        server::copy_session_artifacts,
        store::{ArtifactEdit, MAX_ARTIFACT_BYTES, MAX_ARTIFACT_STATE, apply_edits, free_id},
        tool::{ArtifactBackend, BoxBackendFuture, create_artifact_extension},
    },
    runtime::server::ServerServices,
};

fn is_forbidden<T>(result: &Result<T>) -> bool {
    matches!(
        result,
        Err(Error::Chord(err)) if matches!(err.as_ref(), ChordError { code, .. } if code == "forbidden")
    )
}

async fn server_host(dir: &TempDir) -> Result<(Host, StoreEngine)> {
    let db =
        SqliteDatabase::open(dir.path().join("server.sqlite"), SqliteOptions::default()).await?;
    let store = StoreEngine::open(db).await?;
    let host = create_host(HostOptions {
        principal_policy: Some(Principal::POLICY),
        principal: Some(json!({"kind": "server"})),
        address: Some(JsonValue::from_serde(server_address())),
        store: Some(store.clone()),
        ..Default::default()
    })?;
    let mut registry = PluginRegistry::new();
    register(&mut registry)?;
    let facets: Vec<PluginFacet> = registry.facets(FacetKind::Server, &["artifacts".into()]);
    let report = host.load(facets, &Context::background()).await?;
    assert!(report.failed.is_empty(), "failed: {:?}", report.failed);
    Ok((host, store))
}

#[test]
fn frame_page_injects_scripts_after_head_or_at_document_start() {
    let with_head =
        "<!doctype html><html><HEAD lang=\"en\"><title>T</title></head><body>ok</body></html>";
    let framed = frame_page(with_head);
    let head_end = with_head.find("<title>").expect("title");
    assert!(framed.starts_with(&with_head[..head_end]));
    assert!(framed.contains("p2-artifact-diagnostic"));
    assert!(framed.contains("p2-view"));
    assert!(framed.ends_with("<title>T</title></head><body>ok</body></html>"));

    let fragment = "<div>bare</div>";
    let framed_fragment = frame_page(fragment);
    assert!(framed_fragment.starts_with("<script>"));
    assert!(framed_fragment.ends_with(fragment));
    assert!(FRAME_CSP.contains("sandbox allow-scripts"));
    assert!(FRAME_CSP.contains("'unsafe-eval'"));
}

#[test]
fn apply_edits_and_free_id_enforce_uniqueness_and_non_overlap() {
    let edited = apply_edits(
        "alpha beta gamma",
        &[
            ArtifactEdit {
                old: "gamma".into(),
                new: "GAMMA".into(),
            },
            ArtifactEdit {
                old: "alpha".into(),
                new: "ALPHA".into(),
            },
        ],
    )
    .expect("non-overlapping edits");
    assert_eq!(edited, "ALPHA beta GAMMA");

    assert!(
        apply_edits(
            "dup dup",
            &[ArtifactEdit {
                old: "dup".into(),
                new: "x".into(),
            }],
        )
        .is_err()
    );
    assert!(
        apply_edits(
            "abcdef",
            &[
                ArtifactEdit {
                    old: "abcd".into(),
                    new: "1".into(),
                },
                ArtifactEdit {
                    old: "cdef".into(),
                    new: "2".into(),
                },
            ],
        )
        .is_err()
    );

    let mut taken = std::collections::BTreeMap::new();
    let first = free_id(&taken, "Café Chart!");
    assert_eq!(first, "cafe-chart");
    taken.insert(first, ());
    let second = free_id(&taken, "Café Chart!");
    assert_eq!(second, "cafe-chart-2");
    taken.insert(second, ());
    assert_eq!(free_id(&taken, "Café Chart!"), "cafe-chart-3");
    assert_eq!(free_id(&taken, "!!!"), "artifact");
}

#[tokio::test]
async fn page_registry_tracks_reports_cursors_eval_and_cancellation() {
    let pages = Arc::new(PageRegistry::with_timeout(Duration::from_millis(80)));
    let ctx = Context::background();

    // No open page returns informative error.
    let closed = pages.run("s1", "app", "return 1", &ctx).await;
    assert!(
        matches!(closed, EvalResult::Err { ok: false, ref error } if error.contains("not open"))
    );

    // Report capping & cursor progression.
    let errs: Vec<String> = (0..60).map(|i| format!("err-{i}")).collect();
    let logs: Vec<(String, String)> = (0..5)
        .map(|i| ("log".into(), format!("line-{i}")))
        .collect();
    pages.report("s1", "app", &errs, &logs);
    let kept = pages.errors("s1", "app");
    assert_eq!(kept.len(), 50);
    assert_eq!(kept[0].text, "err-10");
    let (all_logs, next) = pages.logs("s1", "app", 0);
    assert_eq!(all_logs.len(), 5);
    assert_eq!(pages.logs("s1", "app", next).0.len(), 0);

    let (uncarried, marks) = pages.uncarried("s1");
    assert_eq!(uncarried.len(), 50);
    assert_eq!(marks.len(), 1);
    pages.carried("s1", &marks);
    assert!(pages.uncarried("s1").0.is_empty());

    // Open page handles eval request; first answer wins.
    let seen: Arc<Mutex<Vec<EvalRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let pages_answer = Arc::clone(&pages);
    let seen_cb = Arc::clone(&seen);
    let _guard = pages.open(
        "s1",
        "app",
        Arc::new(move |req| {
            seen_cb.lock().expect("lock").push(req.clone());
            pages_answer.answer(&req.request, EvalResult::ok(json!(42)));
            pages_answer.answer(&req.request, EvalResult::ok(json!(99)));
        }),
    );
    let res = pages.run("s1", "app", "return 42", &ctx).await;
    assert_eq!(res, EvalResult::ok(json!(42)));

    // Timeout when page never answers.
    drop(_guard);
    let _silent = pages.open("s1", "app", Arc::new(|_| {}));
    let timed_out = pages.run("s1", "app", "return 1", &ctx).await;
    assert!(
        matches!(timed_out, EvalResult::Err { ok: false, ref error } if error.contains("did not answer"))
    );

    // Cancellation stops waiting immediately.
    let (cancel_ctx, cancel) = Context::background().with_cancel();
    cancel.cancel("stop");
    let cancelled = pages.run("s1", "app", "return 1", &cancel_ctx).await;
    assert_eq!(cancelled, EvalResult::err("Cancelled"));

    pages.forget("s1", "app");
    assert!(pages.errors("s1", "app").is_empty());
}

#[tokio::test]
async fn artifacts_contracts_enforce_auth_persist_across_restart_and_stream_page_eval() -> Result<()>
{
    let dir = TempDir::new()?;
    let ctx = Context::background();
    let artifacts_c = Arc::new(lookup("p2.artifacts")?);
    let page_c = Arc::new(lookup("p2.artifact-page")?);

    let owner = json!({"kind": "owner", "access": "local"});
    let worker_s1 = json!({"kind": "worker", "session": "s1", "generation": 1});
    let worker_s2 = json!({"kind": "worker", "session": "s2", "generation": 1});
    let guest_view = json!({
        "kind": "guest", "id": "gv", "name": "Viewer", "home": "s1",
        "sessions": {"s1": "view"}, "revision": 1
    });
    let guest_full = json!({
        "kind": "guest", "id": "gf", "name": "Editor", "home": "s1",
        "sessions": {"s1": "full"}, "revision": 1
    });

    {
        let (host, _store) = server_host(&dir).await?;

        // Another session's worker is refused.
        let bad = host.hold_as(
            Arc::clone(&artifacts_c),
            JsonValue::from_serde(json!({"session": "s1"})),
            Caller {
                principal: Some(worker_s2),
                incarnation: "w2".into(),
            },
            &ctx,
        )?;
        assert!(is_forbidden(&bad.wait(&ctx).await));

        // Worker writes html and md artifacts, edits html, sets state, checks errors.
        let worker = host.hold_as(
            Arc::clone(&artifacts_c),
            JsonValue::from_serde(json!({"session": "s1"})),
            Caller {
                principal: Some(worker_s1.clone()),
                incarnation: "w1".into(),
            },
            &ctx,
        )?;
        worker.wait(&ctx).await?;
        assert_eq!(worker.variant()?, "worker");

        let written = worker
            .call(
                "write",
                JsonValue::from_serde(json!({
                    "kind": "html",
                    "title": "Counter App",
                    "content": "<html><head></head><body>v1</body></html>"
                })),
                &ctx,
            )
            .await?
            .to_serde()?;
        assert_eq!(written, json!({"id": "counter-app", "version": 1}));

        // Changing kind of an existing artifact is refused.
        assert!(
            worker
                .call(
                    "write",
                    JsonValue::from_serde(json!({
                        "id": "counter-app",
                        "kind": "md",
                        "title": "Counter App",
                        "content": "# nope"
                    })),
                    &ctx,
                )
                .await
                .is_err()
        );

        // Content over 2 MiB is refused.
        let huge = "a".repeat(MAX_ARTIFACT_BYTES + 1);
        assert!(
            worker
                .call(
                    "write",
                    JsonValue::from_serde(json!({
                        "kind": "md",
                        "title": "Huge",
                        "content": huge
                    })),
                    &ctx,
                )
                .await
                .is_err()
        );

        // Edit creates version 2 while version 1 remains readable.
        let edited = worker
            .call(
                "edit",
                JsonValue::from_serde(json!({
                    "id": "counter-app",
                    "edits": [{"old": "<body>v1</body>", "new": "<body>v2</body>"}]
                })),
                &ctx,
            )
            .await?
            .to_serde()?;
        assert_eq!(edited, json!({"version": 2}));
        let v1 = worker
            .call(
                "read",
                JsonValue::from_serde(json!({"id": "counter-app", "version": 1})),
                &ctx,
            )
            .await?
            .to_serde()?;
        assert_eq!(
            v1["content"],
            json!("<html><head></head><body>v1</body></html>")
        );
        let v2 = worker
            .call(
                "read",
                JsonValue::from_serde(json!({"id": "counter-app"})),
                &ctx,
            )
            .await?
            .to_serde()?;
        assert_eq!(v2["version"], json!(2));
        assert_eq!(
            v2["content"],
            json!("<html><head></head><body>v2</body></html>")
        );

        // Markdown artifact rejects state.
        worker
            .call(
                "write",
                JsonValue::from_serde(json!({
                    "id": "notes",
                    "kind": "md",
                    "title": "Notes",
                    "content": "# Hello"
                })),
                &ctx,
            )
            .await?;
        assert!(
            worker
                .call(
                    "setState",
                    JsonValue::from_serde(json!({"id": "notes", "state": {"k": 1}})),
                    &ctx,
                )
                .await
                .is_err()
        );

        // Oversized html state is refused; valid html state is saved.
        let big_state = "x".repeat(MAX_ARTIFACT_STATE + 1);
        assert!(
            worker
                .call(
                    "setState",
                    JsonValue::from_serde(json!({"id": "counter-app", "state": big_state})),
                    &ctx,
                )
                .await
                .is_err()
        );
        worker
            .call(
                "setState",
                JsonValue::from_serde(json!({"id": "counter-app", "state": {"count": 7}})),
                &ctx,
            )
            .await?;
        let state = worker
            .call(
                "readState",
                JsonValue::from_serde(json!({"id": "counter-app"})),
                &ctx,
            )
            .await?
            .to_serde()?;
        assert_eq!(state, json!({"state": {"count": 7}}));

        // Reader vs Editor variants.
        let reader = host.hold_as(
            Arc::clone(&artifacts_c),
            JsonValue::from_serde(json!({"session": "s1"})),
            Caller {
                principal: Some(guest_view.clone()),
                incarnation: "gv".into(),
            },
            &ctx,
        )?;
        reader.wait(&ctx).await?;
        assert_eq!(reader.variant()?, "reader");
        assert!(
            reader
                .call(
                    "remove",
                    JsonValue::from_serde(json!({"id": "notes"})),
                    &ctx
                )
                .await
                .is_err()
        );

        let editor = host.hold_as(
            Arc::clone(&artifacts_c),
            JsonValue::from_serde(json!({"session": "s1"})),
            Caller {
                principal: Some(guest_full.clone()),
                incarnation: "gf".into(),
            },
            &ctx,
        )?;
        editor.wait(&ctx).await?;
        assert_eq!(editor.variant()?, "editor");

        // Open ArtifactPage receives eval request event and answers back to worker.
        let page = host.hold_as(
            Arc::clone(&page_c),
            JsonValue::from_serde(json!({"session": "s1", "id": "counter-app"})),
            Caller {
                principal: Some(owner.clone()),
                incarnation: "owner-1".into(),
            },
            &ctx,
        )?;
        page.wait(&ctx).await?;
        assert_eq!(page.variant()?, "editor");
        assert_eq!(page.read("value")?.to_serde()?, json!({"count": 7}));

        let page_for_cb = page.clone();
        let _sub = page.subscribe_event("request", move |payload| {
            let page_for_cb = page_for_cb.clone();
            if let Ok(val) = payload.to_serde()
                && let Some(req_id) = val["request"].as_str().map(str::to_owned)
            {
                tokio::spawn(async move {
                    let _ = page_for_cb
                        .call(
                            "answer",
                            JsonValue::from_serde(json!({
                                "request": req_id,
                                "result": {"ok": true, "value": {"doubled": 14}}
                            })),
                            &Context::background(),
                        )
                        .await;
                });
            }
        })?;

        // Give on_live routine a tick to register the page channel.
        tokio::time::sleep(Duration::from_millis(25)).await;
        let eval_out = worker
            .call(
                "eval",
                JsonValue::from_serde(
                    json!({"id": "counter-app", "code": "return { doubled: 14 }"}),
                ),
                &ctx,
            )
            .await?
            .to_serde()?;
        assert_eq!(eval_out, json!({"ok": true, "value": {"doubled": 14}}));

        // Page reports an error and a log; worker sees newErrors once, then carried clears it.
        page.call(
            "report",
            JsonValue::from_serde(json!({
                "errors": ["ReferenceError: boom"],
                "logs": [{"level": "log", "text": "hello"}]
            })),
            &ctx,
        )
        .await?;
        let new_errs = worker
            .call("newErrors", JsonValue::Null, &ctx)
            .await?
            .to_serde()?;
        assert_eq!(new_errs["errors"].as_array().map(Vec::len), Some(1));
        assert_eq!(new_errs["errors"][0]["title"], json!("Counter App"));
        let marks: Vec<ErrorMark> = serde_json::from_value(new_errs["marks"].clone())?;
        worker
            .call(
                "carried",
                JsonValue::from_serde(json!({"marks": marks})),
                &ctx,
            )
            .await?;
        let after = worker
            .call("newErrors", JsonValue::Null, &ctx)
            .await?
            .to_serde()?;
        assert_eq!(after["errors"].as_array().map(Vec::len), Some(0));

        // Copy session s1 -> s2 carries all versions and state.
        copy_session_artifacts(&host, "s1", "s2", &ctx).await?;
        let w2 = host.hold_as(
            Arc::clone(&artifacts_c),
            JsonValue::from_serde(json!({"session": "s2"})),
            Caller {
                principal: Some(json!({"kind": "worker", "session": "s2", "generation": 1})),
                incarnation: "w2".into(),
            },
            &ctx,
        )?;
        w2.wait(&ctx).await?;
        let copied_v1 = w2
            .call(
                "read",
                JsonValue::from_serde(json!({"id": "counter-app", "version": 1})),
                &ctx,
            )
            .await?
            .to_serde()?;
        assert_eq!(
            copied_v1["content"],
            json!("<html><head></head><body>v1</body></html>")
        );
        let copied_state = w2
            .call(
                "readState",
                JsonValue::from_serde(json!({"id": "counter-app"})),
                &ctx,
            )
            .await?
            .to_serde()?;
        assert_eq!(copied_state, json!({"state": {"count": 7}}));

        // Editor removes notes; reader/worker no longer sees it.
        editor
            .call(
                "remove",
                JsonValue::from_serde(json!({"id": "notes"})),
                &ctx,
            )
            .await?;
        assert!(
            worker
                .call("read", JsonValue::from_serde(json!({"id": "notes"})), &ctx)
                .await
                .is_err()
        );

        host.close(&ctx).await?;
    }

    // Reopen SQLite store: artifacts, versions, and html state survive restart!
    {
        let (host, _store) = server_host(&dir).await?;
        let worker = host.hold_as(
            Arc::clone(&artifacts_c),
            JsonValue::from_serde(json!({"session": "s1"})),
            Caller {
                principal: Some(worker_s1),
                incarnation: "w1-restarted".into(),
            },
            &ctx,
        )?;
        worker.wait(&ctx).await?;
        let read = worker
            .call(
                "read",
                JsonValue::from_serde(json!({"id": "counter-app", "version": 2})),
                &ctx,
            )
            .await?
            .to_serde()?;
        assert_eq!(
            read["content"],
            json!("<html><head></head><body>v2</body></html>")
        );
        let state = worker
            .call(
                "readState",
                JsonValue::from_serde(json!({"id": "counter-app"})),
                &ctx,
            )
            .await?
            .to_serde()?;
        assert_eq!(state, json!({"state": {"count": 7}}));
        host.close(&ctx).await?;
    }

    Ok(())
}

#[tokio::test]
async fn artifact_frame_route_enforces_csp_auth_and_head() -> Result<()> {
    let dir = TempDir::new()?;
    let ctx = Context::background();
    let mut options = ServerOptions::new(dir.path().to_path_buf());
    options.port = 0;
    register(&mut options.registry)?;
    // Test authenticator: authenticate as local owner when `x-test-auth: owner` header is set.
    let mut auth = ServerServices::default();
    auth.authenticators.insert(
        "test-auth".into(),
        Arc::new(|req, _| {
            Box::pin(async move {
                if req.header("x-test-auth") == Some("owner") {
                    Ok(Some(Principal::Owner {
                        access: OwnerAccess::Local,
                    }))
                } else {
                    Ok(None)
                }
            })
        }),
    );
    options
        .registry
        .register("test-auth", FacetKind::Server, auth.facet())?;
    let server = start_server(options)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;

    let artifacts_c = Arc::new(lookup("p2.artifacts")?);
    let worker = server.host.hold_as(
        artifacts_c,
        JsonValue::from_serde(json!({"session": "s1"})),
        Caller {
            principal: Some(json!({"kind": "worker", "session": "s1", "generation": 1})),
            incarnation: "w1".into(),
        },
        &ctx,
    )?;
    worker.wait(&ctx).await?;
    worker
        .call(
            "write",
            JsonValue::from_serde(json!({
                "id": "demo",
                "kind": "html",
                "title": "Demo",
                "content": "<html><head></head><body>hi</body></html>"
            })),
            &ctx,
        )
        .await?;
    worker
        .call(
            "write",
            JsonValue::from_serde(json!({
                "id": "readme",
                "kind": "md",
                "title": "Readme",
                "content": "# hi"
            })),
            &ctx,
        )
        .await?;

    let client = reqwest::Client::new();
    let base = format!("http://{}", server.address);

    // Unauthenticated -> 403.
    let unauth = client
        .get(format!(
            "{base}/artifact-frame?session=s1&id=demo&version=1"
        ))
        .send()
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    assert_eq!(unauth.status().as_u16(), 403);

    // Cross-site sec-fetch-site -> 403 even for owner.
    let cross = client
        .get(format!(
            "{base}/artifact-frame?session=s1&id=demo&version=1"
        ))
        .header("x-test-auth", "owner")
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    assert_eq!(cross.status().as_u16(), 403);

    // Markdown artifact -> 404.
    let md_resp = client
        .get(format!(
            "{base}/artifact-frame?session=s1&id=readme&version=1"
        ))
        .header("x-test-auth", "owner")
        .send()
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    assert_eq!(md_resp.status().as_u16(), 404);

    // Owner GET html artifact -> 200 with CSP, no-store, injected scripts.
    let ok_resp = client
        .get(format!(
            "{base}/artifact-frame?session=s1&id=demo&version=1"
        ))
        .header("x-test-auth", "owner")
        .send()
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    assert_eq!(ok_resp.status().as_u16(), 200);
    assert_eq!(
        ok_resp
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let csp = ok_resp
        .headers()
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(csp.contains("frame-ancestors 'self'"));
    assert!(csp.contains("sandbox allow-scripts"));
    let body = ok_resp
        .bytes()
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("p2-view"));
    assert!(text.contains("<body>hi</body>"));

    // Owner HEAD returns 200 with empty body and matching Content-Length.
    let head_resp = client
        .head(format!(
            "{base}/artifact-frame?session=s1&id=demo&version=1"
        ))
        .header("x-test-auth", "owner")
        .send()
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    assert_eq!(head_resp.status().as_u16(), 200);
    assert_eq!(
        head_resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok()),
        Some(body.len().to_string().as_str())
    );
    let head_body = head_resp
        .bytes()
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    assert!(head_body.is_empty());

    server
        .close()
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(())
}

struct DummyBackend;
impl ArtifactBackend for DummyBackend {
    fn list<'a>(
        &'a self,
        _: &'a Context,
    ) -> BoxBackendFuture<'a, Vec<crate::plugins::artifacts::store::ArtifactSummary>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn read<'a>(
        &'a self,
        _: &'a str,
        _: Option<i64>,
        _: &'a Context,
    ) -> BoxBackendFuture<'a, crate::plugins::artifacts::store::ArtifactRead> {
        Box::pin(async { Err(Error::Invalid("unimplemented".into())) })
    }
    fn write<'a>(
        &'a self,
        _: crate::plugins::artifacts::store::WriteInput,
        _: &'a Context,
    ) -> BoxBackendFuture<'a, (String, i64)> {
        Box::pin(async { Err(Error::Invalid("unimplemented".into())) })
    }
    fn edit<'a>(
        &'a self,
        _: &'a str,
        _: Vec<ArtifactEdit>,
        _: &'a Context,
    ) -> BoxBackendFuture<'a, i64> {
        Box::pin(async { Err(Error::Invalid("unimplemented".into())) })
    }
    fn remove<'a>(&'a self, _: &'a str, _: &'a Context) -> BoxBackendFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn state<'a>(&'a self, _: &'a str, _: &'a Context) -> BoxBackendFuture<'a, Value> {
        Box::pin(async { Ok(Value::Null) })
    }
    fn set_state<'a>(&'a self, _: &'a str, _: Value, _: &'a Context) -> BoxBackendFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn errors<'a>(
        &'a self,
        _: &'a str,
        _: &'a Context,
    ) -> BoxBackendFuture<'a, Vec<crate::plugins::artifacts::pages::LogEntry>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn logs<'a>(
        &'a self,
        _: &'a str,
        _: Option<i64>,
        _: &'a Context,
    ) -> BoxBackendFuture<'a, (Vec<crate::plugins::artifacts::pages::LogEntry>, i64)> {
        Box::pin(async { Ok((Vec::new(), 0)) })
    }
    fn run<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a Context,
    ) -> BoxBackendFuture<'a, EvalResult> {
        Box::pin(async { Ok(EvalResult::err("none")) })
    }
}

#[test]
fn artifact_extension_registers_all_twelve_code_mode_tools() {
    let ext = create_artifact_extension(Arc::new(DummyBackend));
    assert_eq!(ext.name, "p2-artifacts");
    let names: Vec<&str> = ext
        .tools
        .iter()
        .map(|t| t.declaration.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "artifact_list",
            "artifact_read",
            "artifact_write",
            "artifact_edit",
            "artifact_show",
            "artifact_open",
            "artifact_delete",
            "artifact_html_errors",
            "artifact_html_logs",
            "artifact_html_eval",
            "artifact_html_state",
            "artifact_html_set_state",
        ]
    );
    for tool in &ext.tools {
        assert_eq!(tool.callers.as_deref(), Some(&["tools".to_owned()][..]));
    }
}
