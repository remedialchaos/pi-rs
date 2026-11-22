use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    context::Context,
    contracts::{Contract, Member, schema::*},
    host::*,
    json::JsonValue,
    link::memory_link_pair,
    remote::{RemoteOptions, RemotePeer},
    state::{Batch, Source},
};
use std::{sync::Arc, time::Duration};
fn remote_contract(name: &str, members: IndexMap<String, Member>) -> Result<Arc<Contract>> {
    Ok(Arc::new(Contract::remote(
        name,
        1,
        vec![],
        IndexMap::from([("main".into(), members)]),
        Arc::new(|_| {
            pi_rs_chord::contracts::HostKind::new(
                "server",
                pi_rs_chord::contracts::read_params(&[], "server")?,
            )?
            .address(empty())
        }),
    )?))
}
fn remote_host() -> Result<Host> {
    create_host(HostOptions {
        principal: Some(serde_json::json!("server")),
        address: Some(JsonValue::from_serde(
            serde_json::json!({"kind":"server", "params":{}}),
        )),
        ..Default::default()
    })
}
fn empty() -> JsonValue {
    JsonValue::Object(IndexMap::new())
}
async fn deadline<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .map_err(|_| pi_rs_chord::Error::Invalid("remote deadline".into()))?
}
#[tokio::test]
async fn memory_peer_calls_subscriptions_deltas_and_reconnect() -> Result<()> {
    let ty = remote_contract(
        "counter",
        IndexMap::from([
            ("value".into(), Member::State(int())),
            ("add".into(), method(Some(int()), Some(int()))),
        ]),
    )?;
    let host = remote_host()?;
    let source = Source::new(host.batch(), JsonValue::Number(1.0))?;
    let writable = source.clone();
    let released = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let release_count = released.clone();
    let contract = ty.clone();
    host.load(
        vec![PluginFacet::new(
            "counter",
            define_facet(move |setup| {
                let source = writable.clone();
                let released = release_count.clone();
                setup.serve(
                    contract.clone(),
                    move |_, _, self_| {
                        let released = released.clone();
                        self_.open(
                            |_| Box::pin(async { Ok(()) }),
                            move |_, _| {
                                Box::pin(async move {
                                    released.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    Ok(())
                                })
                            },
                        );
                        let mut members = Members::variant("main");
                        members.state("value", source.clone())?;
                        let source = source.clone();
                        members.method("add", move |args, _| {
                            let source = source.clone();
                            Box::pin(async move {
                                let current = source.value()?.to_json();
                                let (JsonValue::Number(current), JsonValue::Number(amount)) =
                                    (current, args)
                                else {
                                    return Err(pi_rs_chord::Error::Invalid("number".into()));
                                };
                                let next = JsonValue::Number(current + amount);
                                source.set(next.clone(), &Context::background())?;
                                Ok(next)
                            })
                        })?;
                        Ok(members)
                    },
                    ServeOptions::default(),
                )
            }),
        )],
        &Context::background(),
    )
    .await?;
    let (a, b) = memory_link_pair();
    let server = host.accept(a, RemoteOptions::new(serde_json::json!("user")))?;
    let client = RemotePeer::connect(
        b,
        RemoteOptions::new(serde_json::json!("server")),
        Batch::new(|_| {}),
    )?;
    let reference = client.hold(ty, empty(), &Context::background())?;
    assert!(
        client
            .hold(
                Arc::new(Contract::local("private", 1, vec![], IndexMap::new())?),
                empty(),
                &Context::background()
            )
            .is_err()
    );
    deadline(reference.wait(&Context::background()))
        .await
        .map_err(|e| pi_rs_chord::Error::Invalid(format!("initial bind: {e}")))?;
    assert_eq!(
        reference.member("value")?.value()?.to_json(),
        JsonValue::Number(1.0)
    );
    assert_eq!(
        deadline(reference.call("add", JsonValue::Number(4.0), &Context::background()))
            .await
            .map_err(|e| pi_rs_chord::Error::Invalid(format!("call: {e}")))?,
        JsonValue::Number(5.0)
    );
    deadline(async {
        loop {
            if reference.member("value")?.value()?.to_json() == JsonValue::Number(5.0) {
                return Ok(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let (a, b) = memory_link_pair();
    server.reconnect(a)?;
    deadline(async {
        while released.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        Ok(())
    })
    .await?;
    client.reconnect(b)?;
    deadline(reference.wait(&Context::background()))
        .await
        .map_err(|e| {
            pi_rs_chord::Error::Invalid(format!(
                "rebind: {e}, state {:?}",
                reference.state().value()
            ))
        })?;
    assert_eq!(
        reference.member("value")?.value()?.to_json(),
        JsonValue::Number(5.0)
    );
    reference.release();
    assert!(
        reference
            .call("add", JsonValue::Number(1.0), &Context::background())
            .await
            .is_err()
    );
    client.close();
    server.close();
    host.close(&Context::background()).await?;
    Ok(())
}

#[tokio::test]
async fn replica_batches_publish_related_members_atomically_past_the_ack_window() -> Result<()> {
    let contract = remote_contract(
        "paired",
        IndexMap::from([
            ("left".into(), Member::State(int())),
            ("right".into(), Member::State(int())),
        ]),
    )?;
    let host = remote_host()?;
    let left = Source::new(host.batch(), JsonValue::Number(0.0))?;
    let right = Source::new(host.batch(), JsonValue::Number(0.0))?;
    let (l, r, ty) = (left.clone(), right.clone(), contract.clone());
    host.load(
        vec![PluginFacet::new(
            "paired",
            define_facet(move |setup| {
                let (l, r) = (l.clone(), r.clone());
                setup.serve(
                    ty.clone(),
                    move |_, _, _| {
                        let mut members = Members::variant("main");
                        members.state("left", l.clone())?;
                        members.state("right", r.clone())?;
                        Ok(members)
                    },
                    ServeOptions::default(),
                )
            }),
        )],
        &Context::background(),
    )
    .await?;
    let (a, b) = memory_link_pair();
    // The host owns accepted peers even when the caller drops the returned handle.
    drop(host.accept(a, RemoteOptions::new(serde_json::json!("reader")))?);
    let client = RemotePeer::connect(
        b,
        RemoteOptions::new(serde_json::json!("server")),
        Batch::new(|_| {}),
    )?;
    let held = client.hold(contract, empty(), &Context::background())?;
    deadline(held.wait(&Context::background())).await?;
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mismatches = observed.clone();
    let remote_right = held.member("right")?;
    let _watch = held.member("left")?.subscribe(
        move |call, _| {
            if let pi_rs_chord::watch::WatchCall::Value { value, .. } = call {
                mismatches
                    .lock()
                    .unwrap()
                    .push((value, remote_right.value()?.to_json()));
            }
            Ok(())
        },
        |_| {},
    );
    for n in 1..=64 {
        let (l, r) = (left.clone(), right.clone());
        host.batch().within(&Context::background(), move || {
            l.set(JsonValue::Number(n as f64), &Context::background())?;
            r.set(JsonValue::Number(n as f64), &Context::background())
        })?;
    }
    deadline(async {
        while held.member("right")?.value()?.to_json() != JsonValue::Number(64.0) {
            tokio::task::yield_now().await;
        }
        Ok(())
    })
    .await?;
    let values = observed.lock().unwrap().clone();
    assert!(!values.is_empty());
    assert!(
        values.iter().all(|(l, r)| l == r),
        "torn replica batch: {values:?}"
    );
    held.release();
    client.close();
    host.close(&Context::background()).await?;
    Ok(())
}

#[tokio::test]
async fn child_links_stamp_principals_and_abort_drops_the_server_call() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct CallDrop(Arc<AtomicBool>);
    impl Drop for CallDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let ty = remote_contract(
        "caller",
        IndexMap::from([
            ("principal".into(), method(None, Some(str()))),
            ("slow".into(), method(None, None)),
        ]),
    )?;
    let (contract, began, ended) = (ty.clone(), started.clone(), dropped.clone());
    let host = remote_host()?;
    host.load(
        vec![PluginFacet::new(
            "caller",
            define_facet(move |setup| {
                let (began, ended) = (began.clone(), ended.clone());
                setup.serve(
                    contract.clone(),
                    move |_, _, _| {
                        let mut members = Members::variant("main");
                        members.method("principal", |_, ctx| {
                            Box::pin(async move {
                                Ok(JsonValue::from_serde(
                                    ctx.principal.unwrap_or(serde_json::Value::Null),
                                ))
                            })
                        })?;
                        let (began, ended) = (began.clone(), ended.clone());
                        members.method("slow", move |_, _| {
                            let (began, ended) = (began.clone(), ended.clone());
                            Box::pin(async move {
                                let _drop = CallDrop(ended);
                                began.store(true, Ordering::Release);
                                std::future::pending::<()>().await;
                                Ok(JsonValue::Null)
                            })
                        })?;
                        Ok(members)
                    },
                    ServeOptions::default(),
                )
            }),
        )],
        &Context::background(),
    )
    .await?;
    let (a, b) = memory_link_pair();
    let server = host.accept(a, RemoteOptions::new(serde_json::json!("authenticated")))?;
    let client = RemotePeer::connect(
        b,
        RemoteOptions::new(serde_json::json!("forged")),
        Batch::new(|_| {}),
    )?;
    let held = client.hold(ty, empty(), &Context::background())?;
    deadline(held.wait(&Context::background())).await?;
    assert_eq!(
        deadline(held.call("principal", JsonValue::Null, &Context::background())).await?,
        JsonValue::String("authenticated".into())
    );
    let (ctx, cancel) = Context::background().with_cancel();
    let remote = held.clone();
    let call = tokio::spawn(async move { remote.call("slow", JsonValue::Null, &ctx).await });
    deadline(async {
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        Ok(())
    })
    .await?;
    cancel.cancel("test caller cancelled");
    assert!(
        call.await
            .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?
            .is_err()
    );
    deadline(async {
        while !dropped.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        Ok(())
    })
    .await?;
    held.release();
    client.close();
    server.close();
    host.close(&Context::background()).await?;
    Ok(())
}
