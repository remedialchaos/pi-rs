use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    context::Context,
    contracts::{Contract, Member, schema::*},
    host::*,
    json::JsonValue,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
fn contract(name: &str) -> Result<Arc<Contract>> {
    Ok(Arc::new(Contract::local(
        name,
        1,
        vec![],
        IndexMap::from([
            ("value".into(), Member::State(int())),
            ("read".into(), method(None, Some(int()))),
        ]),
    )?))
}
fn facet(contract: Arc<Contract>, value: i64) -> Facet {
    define_facet(move |host| {
        let contract = contract.clone();
        host.serve(
            contract,
            move |_, _, self_| {
                let mut members = Members::new();
                members.state("value", self_.writable(JsonValue::Number(value as f64))?)?;
                members.method("read", move |_, _| {
                    Box::pin(async move { Ok(JsonValue::Number(value as f64)) })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )
    })
}
#[tokio::test]
async fn host_activates_singletons_tracks_presence_and_releases_scopes() -> Result<()> {
    let ty = contract("counter")?;
    let host = create_host(HostOptions::default())?;
    let ctx = Context::background();
    host.load(
        vec![PluginFacet::new("counter", facet(ty.clone(), 3))],
        &ctx,
    )
    .await?;
    let root = host.root();
    let reference = root.hold(ty.clone(), JsonValue::Object(Default::default()))?;
    tokio::time::timeout(std::time::Duration::from_secs(3), reference.wait(&ctx))
        .await
        .unwrap()?;
    assert_eq!(reference.read("value")?, JsonValue::Number(3.0));
    assert_eq!(
        reference.call("read", JsonValue::Null, &ctx).await?,
        JsonValue::Number(3.0)
    );
    assert_eq!(host.references("counter").value().len(), 1);
    root.close();
    assert!(reference.call("read", JsonValue::Null, &ctx).await.is_err());
    assert!(host.references("counter").value().is_empty());
    host.close(&ctx).await?;
    Ok(())
}
#[tokio::test]
async fn reload_preserves_unchanged_facet_identity_and_restarts_changed_objects() -> Result<()> {
    let ty = contract("counter")?;
    let host = create_host(HostOptions::default())?;
    let ctx = Context::background();
    let first = facet(ty.clone(), 1);
    host.load(vec![PluginFacet::new("counter", first.clone())], &ctx)
        .await?;
    let held = host.hold(ty.clone(), JsonValue::Object(Default::default()), &ctx)?;
    tokio::time::timeout(std::time::Duration::from_secs(3), held.wait(&ctx))
        .await
        .unwrap()?;
    let readable_before_reload = held.member("value")?;
    let report = host
        .reload(vec![PluginFacet::new("counter", first)], &ctx)
        .await?;
    assert!(report.changed.is_empty());
    let report = host
        .reload(vec![PluginFacet::new("counter", facet(ty, 2))], &ctx)
        .await?;
    assert_eq!(report.changed, vec!["counter"]);
    tokio::time::timeout(std::time::Duration::from_secs(3), held.wait(&ctx))
        .await
        .unwrap()?;
    assert_eq!(held.read("value")?, JsonValue::Number(2.0));
    assert_eq!(
        readable_before_reload.value()?.to_json(),
        JsonValue::Number(2.0)
    );
    host.close(&ctx).await?;
    Ok(())
}
#[tokio::test]
async fn failed_plugin_holds_back_transitive_unbindable_reload_closure() -> Result<()> {
    let a = contract("a")?;
    let b = contract("b")?;
    let c = contract("c")?;
    let host = create_host(HostOptions::default())?;
    let ctx = Context::background();
    host.load(
        vec![
            PluginFacet::new("a", facet(a.clone(), 1)),
            PluginFacet::new("b", facet(b.clone(), 1)),
            PluginFacet::new("c", facet(c.clone(), 1)),
        ],
        &ctx,
    )
    .await?;
    let old_a = a.clone();
    let next_a = Arc::new(a.evolve(|edit| {
        edit.put("members", "extra", Member::State(int()), true)?;
        Ok(())
    })?);
    let next_b = Arc::new(b.evolve(|edit| {
        edit.put("members", "extra", Member::State(int()), true)?;
        Ok(())
    })?);
    let b_request = next_a.clone();
    let c_request = next_b.clone();
    let bf = define_facet(move |setup| {
        let requested = b_request.clone();
        setup.serve(
            next_b.clone(),
            move |_, _, self_| {
                self_.get(requested.clone(), JsonValue::Object(Default::default()))?;
                Ok(Members::new())
            },
            ServeOptions::default(),
        )
    });
    let cf = define_facet(move |setup| {
        let requested = c_request.clone();
        setup.serve(
            c.clone(),
            move |_, _, self_| {
                self_.get(requested.clone(), JsonValue::Object(Default::default()))?;
                Ok(Members::new())
            },
            ServeOptions::default(),
        )
    });
    let report = host
        .reload(
            vec![
                PluginFacet::new(
                    "a",
                    define_facet(|_| Err(pi_rs_chord::Error::Invalid("bad build".into()))),
                ),
                PluginFacet::new("b", bf),
                PluginFacet::new("c", cf),
            ],
            &ctx,
        )
        .await?;
    assert_eq!(
        report
            .failed
            .iter()
            .map(|e| e.plugin.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["a", "b", "c"])
    );
    assert!(report.changed.is_empty());
    let held = host.hold(old_a, JsonValue::Object(Default::default()), &ctx)?;
    tokio::time::timeout(std::time::Duration::from_secs(3), held.wait(&ctx))
        .await
        .unwrap()?;
    assert_eq!(held.read("value")?, JsonValue::Number(1.0));
    host.close(&ctx).await?;
    Ok(())
}
#[tokio::test]
async fn staging_records_declarations_without_running_openers_or_routines() -> Result<()> {
    let ty = contract("counter")?;
    let host = create_host(HostOptions::default())?;
    let ctx = Context::background();
    let opened = Arc::new(AtomicUsize::new(0));
    host.load(
        vec![PluginFacet::new("counter", facet(ty.clone(), 1))],
        &ctx,
    )
    .await?;
    let count = opened.clone();
    let changed = define_facet(move |host| {
        let count = count.clone();
        host.serve(
            ty.clone(),
            move |_, _, self_| {
                let count = count.clone();
                self_.on_live(move |_| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(()) })
                });
                Err(pi_rs_chord::Error::Invalid("rehearsal fails".into()))
            },
            ServeOptions::default(),
        )
    });
    let report = host
        .reload(vec![PluginFacet::new("counter", changed)], &ctx)
        .await?;
    assert_eq!(report.failed.len(), 1);
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    host.close(&ctx).await?;
    Ok(())
}
#[test]
fn reload_cycles_are_cut_when_later_edges_close_a_drain_cycle() -> Result<()> {
    let cycles = ReloadCycles::default();
    cycles.set_edges("a", vec!["b".into()]);
    let waiting = cycles.watch("a", vec!["old".into()]);
    assert!(waiting.error().is_none());
    cycles.set_edges("b", vec!["old".into()]);
    let error = waiting.error().unwrap();
    assert_eq!(error.code, "unavailable");
    assert_eq!(error.reason.as_deref(), Some("reload_cycle"));
    Ok(())
}

#[tokio::test]
async fn rejected_rehearsal_has_no_document_table_job_contribution_or_request_effects() -> Result<()>
{
    use pi_rs_chord::{
        documents::Definition, jobs::*, registries::*, sqlite::SqliteDatabase, store::*,
    };
    let db = SqliteDatabase::open(":memory:", Default::default()).await?;
    let store = StoreEngine::open(db.clone()).await?;
    let host = create_host(HostOptions {
        store: Some(store),
        ..Default::default()
    })?;
    let ctx = Context::background();
    let ty = contract("counter")?;
    let registry = define_registry::<JsonValue, JsonValue>("catalog", 1)?;
    let declared = registry.clone();
    let provider = define_facet(move |setup| {
        setup.serve_registry(
            declared.clone(),
            RegistryProvider::<JsonValue, JsonValue>::new(|entries, _| {
                Ok(JsonValue::Array(
                    entries.iter().map(|e| e.value.clone()).collect(),
                ))
            }),
        )?;
        setup.contribute(declared.clone(), JsonValue::String("kept".into()))
    });
    host.load(
        vec![
            PluginFacet::new("catalog", provider.clone()),
            PluginFacet::new("counter", facet(ty.clone(), 1)),
        ],
        &ctx,
    )
    .await?;
    let before = db
        .call(|c| {
            Ok(
                c.query_row("SELECT COUNT(*) FROM chord_documents", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            )
        })
        .await?;
    let effects = Arc::new(AtomicUsize::new(0));
    let initial_count = effects.clone();
    let document = Arc::new(
        Definition::new("new_state", obj::<&str>([]), 1, move || {
            initial_count.fetch_add(1, Ordering::SeqCst);
            Ok(JsonValue::Object(Default::default()))
        })?
        .persisted(true, |_, _, _| false),
    );
    let table = TableDefinition::new(
        "new_table",
        1,
        IndexMap::from([("id".into(), ColumnRecord::new(ColumnType::Int))]),
        vec!["id".into()],
        IndexMap::new(),
    )?;
    let job = define_job(
        "new_job",
        JobOptions {
            persist: Some(Resume::Rerun),
            ..JobOptions::new(1, int(), int())
        },
    )?;
    let phantom = contract("phantom")?;
    let request = phantom.clone();
    let contributed = registry.clone();
    let touched = effects.clone();
    let rejected = define_facet(move |setup| {
        let (document, table, job, request, contributed, touched) = (
            document.clone(),
            table.clone(),
            job.clone(),
            request.clone(),
            contributed.clone(),
            touched.clone(),
        );
        setup.serve(
            ty.clone(),
            move |_, _, self_| {
                self_.state(document.clone())?;
                self_.table(table.clone())?;
                let touched = touched.clone();
                self_.job(
                    job.clone(),
                    JobImplementation::new(move |_, _| {
                        touched.fetch_add(1, Ordering::SeqCst);
                        Box::pin(async { Ok(JsonValue::Number(1.0)) })
                    }),
                )?;
                self_.contribute(
                    contributed.clone(),
                    JsonValue::String("must not leak".into()),
                )?;
                self_.registry(contributed.clone())?;
                self_.get(request.clone(), JsonValue::Object(Default::default()))?;
                Err(pi_rs_chord::Error::Invalid(
                    "reject after collecting all declarations".into(),
                ))
            },
            ServeOptions::default(),
        )
    });
    let result = host
        .reload(
            vec![
                PluginFacet::new("catalog", provider),
                PluginFacet::new("counter", rejected),
            ],
            &ctx,
        )
        .await?;
    assert_eq!(result.failed.len(), 1);
    assert!(result.changed.is_empty());
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert!(host.references("phantom").value().is_empty());
    assert_eq!(
        host.registry(registry).value(1)?,
        JsonValue::Array(vec![JsonValue::String("kept".into())])
    );
    assert_eq!(
        db.call(|c| Ok(
            c.query_row("SELECT COUNT(*) FROM chord_documents", [], |r| r
                .get::<_, i64>(0))?
        ))
        .await?,
        before
    );
    assert_eq!(
        db.call(|c| Ok(c.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE '%new_table%'",
            [],
            |r| r.get::<_, i64>(0)
        )?))
        .await?,
        0
    );
    host.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
async fn hosted_document_members_activate_before_binding_and_reload_from_committed_state()
-> Result<()> {
    use pi_rs_chord::{
        delta::Segment, documents::Definition, sqlite::SqliteDatabase, store::StoreEngine,
    };
    let db = SqliteDatabase::open(":memory:", Default::default()).await?;
    let store = StoreEngine::open(db).await?;
    let host = create_host(HostOptions {
        store: Some(store),
        ..Default::default()
    })?;
    let contract = Arc::new(Contract::local(
        "document_owner",
        1,
        vec![],
        IndexMap::from([
            ("value".into(), Member::State(obj([("n", required(int()))]))),
            ("set".into(), method(Some(int()), Some(int()))),
        ]),
    )?);
    let definition = Arc::new(
        Definition::new("counter", obj([("n", required(int()))]), 1, || {
            Ok(JsonValue::from_serde(serde_json::json!({"n":0})))
        })?
        .persisted(true, |_, _, _| false),
    );
    let make = || {
        let (contract, definition) = (contract.clone(), definition.clone());
        define_facet(move |setup| {
            let definition = definition.clone();
            setup.serve(
                contract.clone(),
                move |_, _, self_| {
                    let document = self_.state(definition.clone())?;
                    let mut members = Members::new();
                    members.state("value", document.source()?)?;
                    members.method("set", move |args, ctx| {
                        let document = document.clone();
                        Box::pin(async move {
                            let result = args.clone();
                            document
                                .change(&ctx.context, move |draft| {
                                    Box::pin(async move {
                                        draft.set(&[Segment::Key("n".into())], args)?;
                                        Ok(())
                                    })
                                })
                                .await?;
                            Ok(result)
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        })
    };
    let ctx = Context::background();
    host.load(vec![PluginFacet::new("documents", make())], &ctx)
        .await?;
    let held = host.hold(
        contract.clone(),
        JsonValue::Object(Default::default()),
        &ctx,
    )?;
    tokio::time::timeout(std::time::Duration::from_secs(3), held.wait(&ctx))
        .await
        .unwrap()?;
    let readable = held.member("value")?;
    held.call("set", JsonValue::Number(7.0), &ctx).await?;
    assert_eq!(
        readable.value()?.to_json(),
        JsonValue::from_serde(serde_json::json!({"n":7}))
    );
    let report = host
        .reload(vec![PluginFacet::new("documents", make())], &ctx)
        .await?;
    assert_eq!(report.changed, vec!["documents"]);
    assert_eq!(
        readable.value()?.to_json(),
        JsonValue::from_serde(serde_json::json!({"n":7}))
    );
    held.release();
    host.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
async fn admission_changes_followed_values_and_revocation_fence_live_references() -> Result<()> {
    use pi_rs_chord::{
        contracts::Address,
        state::{NativeSource, Source},
    };
    let host = create_host(HostOptions {
        principal: Some(serde_json::json!("user")),
        address: Some(JsonValue::Object(Default::default())),
        ..Default::default()
    })?;
    let ty = Arc::new(Contract::remote(
        "view",
        1,
        vec![],
        IndexMap::from([(
            "main".into(),
            IndexMap::from([("value".into(), Member::State(int()))]),
        )]),
        Arc::new(|_| {
            Ok(Address {
                kind: "server".into(),
                params: JsonValue::Object(Default::default()),
                key: "server:{}".into(),
            })
        }),
    )?);
    let decision = NativeSource::new(host.batch(), deny::<JsonValue>("not admitted"), |a, b| {
        a == b
    });
    let admit = decision.clone();
    let external = Source::new(host.batch(), JsonValue::Number(1.0))?;
    let source = external.clone();
    let closed = Arc::new(AtomicUsize::new(0));
    let cleanups = closed.clone();
    let served = ty.clone();
    host.load(
        vec![PluginFacet::new(
            "view",
            define_facet(move |setup| {
                let source = source.clone();
                let cleanups = cleanups.clone();
                setup.serve(
                    served.clone(),
                    move |_, principal, self_| {
                        assert_eq!(principal, Some(&serde_json::json!("user")));
                        assert!(self_.admitted().is_some());
                        let cleanups = cleanups.clone();
                        self_.open(
                            |_| Box::pin(async { Ok(()) }),
                            move |_, _| {
                                Box::pin(async move {
                                    cleanups.fetch_add(1, Ordering::SeqCst);
                                    Ok(())
                                })
                            },
                        );
                        let mut members = Members::variant("main");
                        members.state("value", self_.follow_source(source.clone())?)?;
                        Ok(members)
                    },
                    ServeOptions {
                        admit: Some(Arc::new({
                            let admit = admit.clone();
                            move |_, _| Ok(admit.clone())
                        })),
                        ..Default::default()
                    },
                )
            }),
        )],
        &Context::background(),
    )
    .await?;
    let held = host.hold(
        ty.clone(),
        JsonValue::Object(Default::default()),
        &Context::background(),
    )?;
    assert!(held.wait(&Context::background()).await.is_err());
    decision.set(
        allow(JsonValue::String("reader".into())),
        &Context::background(),
    )?;
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        held.wait(&Context::background()),
    )
    .await
    .unwrap()?;
    assert_eq!(held.read("value")?, JsonValue::Number(1.0));
    external.set(JsonValue::Number(4.0), &Context::background())?;
    assert_eq!(held.read("value")?, JsonValue::Number(4.0));
    let readable = held.member("value")?;
    decision.set(deny("access removed"), &Context::background())?;
    assert!(held.wait(&Context::background()).await.is_err());
    assert!(readable.value().is_err());
    external.set(JsonValue::Number(5.0), &Context::background())?;
    assert!(readable.value().is_err());
    decision.set(
        allow(JsonValue::String("reader".into())),
        &Context::background(),
    )?;
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        held.wait(&Context::background()),
    )
    .await
    .unwrap()?;
    assert_eq!(readable.value()?.to_json(), JsonValue::Number(5.0));
    host.revoke(&serde_json::json!("user"), &Context::background())
        .await?;
    assert!(held.read("value").is_err());
    assert!(readable.value().is_err());
    assert_eq!(closed.load(Ordering::SeqCst), 2);
    assert!(
        host.hold(
            ty,
            JsonValue::Object(Default::default()),
            &Context::background()
        )
        .is_err()
    );
    host.close(&Context::background()).await?;
    Ok(())
}

#[tokio::test]
async fn an_old_call_waiting_for_its_reloaded_object_is_cut_without_the_drain_timeout() -> Result<()>
{
    let ty = contract("counter")?;
    let next = Arc::new(ty.evolve(|edit| {
        edit.put("members", "read", method(None, Some(int())), false)?;
        Ok(())
    })?);
    let started = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Notify::new());
    let begin = started.clone();
    let run = gate.clone();
    let requested = next.clone();
    let served = ty.clone();
    let host = create_host(HostOptions {
        reload_drain_timeout: std::time::Duration::from_secs(30),
        ..Default::default()
    })?;
    host.load(
        vec![PluginFacet::new(
            "counter",
            define_facet(move |setup| {
                let started = begin.clone();
                let gate = run.clone();
                let requested = requested.clone();
                setup.serve(
                    served.clone(),
                    move |_, _, self_| {
                        let mut members = Members::new();
                        members.state("value", self_.writable(JsonValue::Number(1.0))?)?;
                        let started = started.clone();
                        let gate = gate.clone();
                        let requested = requested.clone();
                        members.method("read", move |_, ctx| {
                            let started = started.clone();
                            let gate = gate.clone();
                            let requested = requested.clone();
                            Box::pin(async move {
                                started.notify_one();
                                gate.notified().await;
                                let held =
                                    ctx.get(requested, JsonValue::Object(Default::default()))?;
                                ctx.call(&held, "read", JsonValue::Null).await
                            })
                        })?;
                        Ok(members)
                    },
                    Default::default(),
                )
            }),
        )],
        &Context::background(),
    )
    .await?;
    let held = host
        .get(
            ty,
            JsonValue::Object(Default::default()),
            &Context::background(),
        )
        .await?;
    let caller = held.clone();
    let call = tokio::spawn(async move {
        caller
            .call("read", JsonValue::Null, &Context::background())
            .await
    });
    started.notified().await;
    let reloader = host.clone();
    let reload = tokio::spawn(async move {
        reloader
            .reload(
                vec![PluginFacet::new("counter", facet(next, 2))],
                &Context::background(),
            )
            .await
    });
    while matches!(held.state().value(), RefState::Live) {
        tokio::task::yield_now().await;
    }
    gate.notify_one();
    let error = tokio::time::timeout(std::time::Duration::from_secs(2), call)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    match error {
        pi_rs_chord::Error::Chord(error) => {
            assert_eq!(error.code, "unavailable");
            assert_eq!(error.reason.as_deref(), Some("reload_cycle"));
        }
        other => panic!("{other}"),
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), reload)
        .await
        .unwrap()
        .unwrap()?;
    host.close(&Context::background()).await?;
    Ok(())
}

#[tokio::test]
async fn registry_contributions_swap_without_a_gap_unless_the_old_object_opens_natives()
-> Result<()> {
    use pi_rs_chord::{registries::*, watch::NativeWatchCall};
    use std::sync::Mutex;
    for native in [false, true] {
        let host = create_host(HostOptions::default())?;
        let ctx = Context::background();
        let token = define_registry::<JsonValue, JsonValue>("handover", 1)?;
        let registry = host.registry(token.clone());
        let served = token.clone();
        let provider = define_facet(move |setup| {
            setup.serve_registry(
                served.clone(),
                RegistryProvider::<JsonValue, JsonValue>::new(|entries, _| {
                    Ok(JsonValue::Array(
                        entries.iter().map(|e| e.value.clone()).collect(),
                    ))
                }),
            )
        });
        let ty = contract("contributor")?;
        let make = |value: Option<f64>| {
            let ty = ty.clone();
            let token = token.clone();
            define_facet(move |setup| {
                let token = token.clone();
                setup.serve(
                    ty.clone(),
                    move |_, _, self_| {
                        if native {
                            self_.open(
                                |_| Box::pin(async { Ok(()) }),
                                |_, _| Box::pin(async { Ok(()) }),
                            );
                        }
                        if let Some(value) = value {
                            self_.contribute(token.clone(), JsonValue::Number(value))?;
                        }
                        if value == Some(-1.0) {
                            self_.follow(|_, _| {
                                Box::pin(async {
                                    Err(pi_rs_chord::data::ChordError::new(
                                        "invalid",
                                        "failed attachment",
                                    )
                                    .into())
                                })
                            })?;
                        }
                        let mut members = Members::new();
                        members.state("value", self_.writable(JsonValue::Number(0.0))?)?;
                        members.method("read", |_, _| {
                            Box::pin(async { Ok(JsonValue::Number(0.0)) })
                        })?;
                        Ok(members)
                    },
                    Default::default(),
                )
            })
        };
        host.load(
            vec![
                PluginFacet::new("provider", provider.clone()),
                PluginFacet::new("owner", make(Some(1.0))),
            ],
            &ctx,
        )
        .await?;
        assert_eq!(
            registry.value(1)?,
            JsonValue::Array(vec![JsonValue::Number(1.0)])
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let _observation = registry.source().observe(
            &ctx,
            move |call, _| {
                if let NativeWatchCall::Value { value: Some(value) } = call {
                    observed.lock().unwrap().push(value);
                }
                Ok(())
            },
            |_| {},
        )?;
        host.reload(
            vec![
                PluginFacet::new("provider", provider.clone()),
                PluginFacet::new("owner", make(Some(2.0))),
            ],
            &ctx,
        )
        .await?;
        let mut expected = Vec::new();
        if native {
            expected.push(JsonValue::Array(Vec::new()));
        }
        expected.push(JsonValue::Array(vec![JsonValue::Number(2.0)]));
        assert_eq!(*seen.lock().unwrap(), expected, "native={native}");
        seen.lock().unwrap().clear();
        host.reload(
            vec![
                PluginFacet::new("provider", provider.clone()),
                PluginFacet::new("owner", make(None)),
            ],
            &ctx,
        )
        .await?;
        assert_eq!(*seen.lock().unwrap(), vec![JsonValue::Array(Vec::new())]);
        assert_eq!(registry.value(1)?, JsonValue::Array(Vec::new()));
        host.reload(
            vec![
                PluginFacet::new("provider", provider.clone()),
                PluginFacet::new("owner", make(Some(3.0))),
            ],
            &ctx,
        )
        .await?;
        seen.lock().unwrap().clear();
        host.reload(
            vec![
                PluginFacet::new("provider", provider),
                PluginFacet::new("owner", make(Some(-1.0))),
            ],
            &ctx,
        )
        .await?;
        let last = if native {
            Vec::new()
        } else {
            vec![JsonValue::Number(3.0)]
        };
        assert_eq!(registry.value(1)?, JsonValue::Array(last));
        host.close(&ctx).await?;
        assert_eq!(
            seen.lock().unwrap().last(),
            Some(&JsonValue::Array(Vec::new()))
        );
    }
    Ok(())
}

#[tokio::test]
async fn named_query_checks_its_argument_before_building_the_store_query() -> Result<()> {
    use pi_rs_chord::store::{ColumnRecord, ColumnType, Query, TableDefinition};
    let contract = Arc::new(Contract::local(
        "lookup",
        1,
        vec![],
        IndexMap::from([(
            "rows".into(),
            queries(int(), IndexMap::from([("recent".into(), int())])),
        )]),
    )?);
    let table = TableDefinition::new(
        "rows",
        1,
        IndexMap::from([("id".into(), ColumnRecord::new(ColumnType::Int))]),
        vec!["id".into()],
        IndexMap::new(),
    )?;
    let provided = contract.clone();
    let host = create_host(HostOptions::default())?;
    let ctx = Context::background();
    host.load(
        vec![PluginFacet::new(
            "lookup",
            define_facet(move |setup| {
                let table = table.clone();
                setup.serve(
                    provided.clone(),
                    move |_, _, _| {
                        let table = table.clone();
                        let mut members = Members::new();
                        let recent: QueryBuilder =
                            Arc::new(move |_| Query::new("lookup", table.clone())?.with_limit(7));
                        members.queries("rows", IndexMap::from([("recent".into(), recent)]))?;
                        Ok(members)
                    },
                    ServeOptions::default(),
                )
            }),
        )],
        &ctx,
    )
    .await?;
    let held = host.hold(contract, JsonValue::Object(Default::default()), &ctx)?;
    held.wait(&ctx).await?;
    assert_eq!(
        held.query("rows", "recent", JsonValue::Number(1.0))?.limit,
        7
    );
    assert!(held.query("rows", "recent", JsonValue::Null).is_err());
    host.close(&ctx).await
}

#[tokio::test]
async fn held_key_tracks_the_same_key_after_provider_reload() -> Result<()> {
    let contract = Arc::new(Contract::local(
        "keyed-values",
        1,
        vec![],
        IndexMap::from([("label".into(), keyed(str(), None))]),
    )?);
    let host = create_host(HostOptions::default())?;
    let ctx = Context::background();
    let facet = |generation: &'static str| {
        let contract = contract.clone();
        let batch = host.batch();
        define_facet(move |setup| {
            let batch = batch.clone();
            setup.serve(
                contract.clone(),
                move |_, _, _| {
                    let mut members = Members::new();
                    let batch = batch.clone();
                    members.keyed("label", move |key| {
                        let JsonValue::String(key) = key else {
                            return Err(pi_rs_chord::Error::Invalid("string key".into()));
                        };
                        pi_rs_chord::state::Source::new(
                            batch.clone(),
                            JsonValue::String(format!("{}-{generation}", key.to_utf8()?).into()),
                        )
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        })
    };
    host.load(vec![PluginFacet::new("keys", facet("first"))], &ctx)
        .await?;
    let held = host.hold(
        contract.clone(),
        JsonValue::Object(Default::default()),
        &ctx,
    )?;
    held.wait(&ctx).await?;
    let source = held.keyed("label", &JsonValue::String("leaf".into()))?;
    assert_eq!(
        source.value()?.to_json(),
        JsonValue::String("leaf-first".into())
    );
    host.reload(vec![PluginFacet::new("keys", facet("second"))], &ctx)
        .await?;
    held.wait(&ctx).await?;
    assert_eq!(
        source.value()?.to_json(),
        JsonValue::String("leaf-second".into())
    );
    held.release();
    assert!(source.value().is_err());
    host.close(&ctx).await
}
