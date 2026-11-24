use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    context::Context,
    contracts::{Contract, Member, schema::*},
    host::*,
    json::JsonValue,
    link::{Link, memory_link_pair},
    remote::{RemoteOptions, RemotePeer},
    state::{Batch, Source},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
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
async fn revoking_a_forwarded_caller_cancels_pending_demand_without_closing_the_parent()
-> Result<()> {
    let contract = remote_contract("pending-demand", IndexMap::new())?;
    let context = Context::background();
    let host = create_host(HostOptions {
        principal_policy: Some(PrincipalPolicy {
            key: pi_rs_chord::data::principal_key,
            revoked_by: |who, selector| who == selector,
        }),
        ..Default::default()
    })?;
    let (a, b) = memory_link_pair();
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let mut parent = RemoteOptions::new(serde_json::json!("parent"));
    parent.trusted = true;
    parent
        .route_contracts
        .insert(contract.name().into(), contract.clone());
    parent.route = Some(Arc::new(move |request| {
        let _ = requests.send(request.context);
        Box::pin(std::future::pending())
    }));
    let serving = host.connect(a, parent)?;
    let mut child = RemoteOptions::new(serde_json::json!("child"));
    child.role = pi_rs_chord::remote::LinkRole::Child;
    child.trusted = true;
    let client = RemotePeer::connect(b, child, Batch::new(|_| {}))?;
    let guest = serde_json::json!({"kind":"guest"});
    let held = client.hold_as(
        contract,
        empty(),
        Caller {
            principal: Some(guest.clone()),
            incarnation: uuid::Uuid::new_v4().to_string(),
        },
        &context,
    )?;
    let demand = tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .map_err(|_| pi_rs_chord::Error::Invalid("route demand timed out".into()))?
        .ok_or(pi_rs_chord::Error::Closed)?;
    host.revoke(&guest, &context).await?;
    client.fence().await?;
    assert!(demand.is_aborted());
    assert!(
        matches!(held.state().value(), RefState::Ended { error } if error.code == "forbidden" && error.reason.as_deref() == Some("revoked"))
    );
    serving.close();
    Ok(())
}

#[tokio::test]
async fn holds_before_handshake_bind_and_replay_after_reconnect() -> Result<()> {
    let ty = remote_contract(
        "handshake",
        IndexMap::from([("get".into(), method(None, Some(int())))]),
    )?;
    let host = remote_host()?;
    let contract = ty.clone();
    host.load(
        vec![PluginFacet::new(
            "handshake",
            define_facet(move |setup| {
                setup.serve(
                    contract.clone(),
                    |_, _, _| {
                        let mut members = Members::variant("main");
                        members
                            .method("get", |_, _| Box::pin(async { Ok(JsonValue::Number(4.)) }))?;
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
    let client = RemotePeer::connect(
        a,
        RemoteOptions::new(serde_json::json!("server")),
        Batch::new(|_| {}),
    )?;
    let context = Context::background();
    let held = client.hold(ty.clone(), empty(), &context)?;
    assert!(matches!(held.state().value(), RefState::Loading));
    let server = RemotePeer::serve(
        host.clone(),
        b,
        RemoteOptions::new(serde_json::json!("user")),
    )?;
    assert_eq!(
        deadline(held.call("get", JsonValue::Null, &context)).await?,
        JsonValue::Number(4.)
    );

    let (a, b) = memory_link_pair();
    client.reconnect(a)?;
    let new_hold = client.hold(ty, empty(), &context)?;
    assert!(matches!(held.state().value(), RefState::Loading));
    assert!(matches!(new_hold.state().value(), RefState::Loading));
    server.reconnect(b)?;
    assert_eq!(
        deadline(held.call("get", JsonValue::Null, &context)).await?,
        JsonValue::Number(4.)
    );
    assert_eq!(
        deadline(new_hold.call("get", JsonValue::Null, &context)).await?,
        JsonValue::Number(4.)
    );
    Ok(())
}

struct GreetingGate {
    link: Arc<dyn Link>,
    during_greeting: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Link for GreetingGate {
    fn send(&self, bytes: Vec<u8>) -> Result<()> {
        let gate = self
            .during_greeting
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        if let Some(gate) = gate {
            gate();
        }
        self.link.send(bytes)
    }
    fn close(&self) {
        self.link.close();
    }
    fn on_message(&self, listener: Arc<dyn Fn(Vec<u8>) + Send + Sync>) {
        self.link.on_message(listener);
    }
    fn on_close(&self, listener: Arc<dyn Fn() + Send + Sync>) {
        self.link.on_close(listener);
    }
    fn closed(&self) -> bool {
        self.link.closed()
    }
}

#[tokio::test]
async fn close_wins_during_a_reconnect_greeting_callback() -> Result<()> {
    let ty = remote_contract("close-during-reconnect", IndexMap::new())?;
    let (original, _other) = memory_link_pair();
    let peer = RemotePeer::connect(
        original,
        RemoteOptions::new(serde_json::json!("server")),
        Batch::new(|_| {}),
    )?;
    let held = peer.hold(ty, empty(), &Context::background())?;
    let (new, _other) = memory_link_pair();
    let (began, greeting) = tokio::sync::oneshot::channel();
    let (resume, paused) = std::sync::mpsc::channel();
    let gated = Arc::new(GreetingGate {
        link: new.clone(),
        during_greeting: Mutex::new(Some(Box::new(move || {
            let _ = began.send(());
            let _ = paused.recv_timeout(Duration::from_secs(10));
        }))),
    });
    let reconnecting = peer.clone();
    let reconnect = tokio::task::spawn_blocking(move || reconnecting.reconnect(gated));
    greeting
        .await
        .map_err(|err| pi_rs_chord::Error::Invalid(err.to_string()))?;
    let replacing = peer.clone();
    let mut close = tokio::task::spawn_blocking(move || {
        let (rival, _other) = memory_link_pair();
        let competing = replacing.reconnect(rival);
        replacing.close();
        competing
    });
    let before_resume = tokio::time::timeout(Duration::from_secs(3), &mut close).await;
    let _ = resume.send(());
    let reconnected = reconnect
        .await
        .map_err(|err| pi_rs_chord::Error::Invalid(err.to_string()))?;
    let competing = match before_resume {
        Ok(result) => result,
        Err(_) => {
            let _ = close.await;
            return Err(pi_rs_chord::Error::Invalid(
                "close was blocked by the greeting callback".into(),
            ));
        }
    }
    .map_err(|err| pi_rs_chord::Error::Invalid(err.to_string()))?;
    assert!(
        matches!(competing, Err(pi_rs_chord::Error::Chord(error)) if error.code == "unavailable")
    );
    assert!(matches!(reconnected, Err(pi_rs_chord::Error::Closed)));
    assert!(new.closed());
    assert!(matches!(held.state().value(), RefState::Ended { .. }));
    Ok(())
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

#[tokio::test]
async fn remote_wire_indexes_state_and_methods_in_sorted_utf16_order() -> Result<()> {
    use pi_rs_chord::link::{
        Body, Channel, ChannelHandler, ChannelOptions, Codec, Limits, Message, Part, Payload,
    };

    // Declare state and method names in reverse alphabetical order to verify wire links
    // sort them canonically (per upstream contracts/contract.ts::variantNames).
    let contract = remote_contract(
        "unsorted",
        IndexMap::from([
            ("zebra".into(), Member::State(str())),
            ("alpha".into(), Member::State(str())),
            (
                "z_call".into(),
                Member::Method {
                    args: nul(),
                    result: str(),
                },
            ),
            (
                "a_call".into(),
                Member::Method {
                    args: nul(),
                    result: str(),
                },
            ),
        ]),
    )?;
    let host = remote_host()?;
    let zebra = Source::new(host.batch(), JsonValue::String("Z".into()))?;
    let alpha = Source::new(host.batch(), JsonValue::String("A".into()))?;
    let ty = contract.clone();
    host.load(
        vec![PluginFacet::new(
            "unsorted",
            define_facet(move |setup| {
                let (zebra, alpha) = (zebra.clone(), alpha.clone());
                setup.serve(
                    ty.clone(),
                    move |_, _, _| {
                        let mut members = Members::variant("main");
                        members.state("zebra", zebra.clone())?;
                        members.state("alpha", alpha.clone())?;
                        members.method("z_call", |_, _| {
                            Box::pin(async { Ok(JsonValue::String("called_z".into())) })
                        })?;
                        members.method("a_call", |_, _| {
                            Box::pin(async { Ok(JsonValue::String("called_a".into())) })
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
    let server = host.accept(a, RemoteOptions::new(serde_json::json!("reader")))?;

    struct Capture(tokio::sync::mpsc::UnboundedSender<Message>);
    impl ChannelHandler for Capture {
        fn opened(&self) {}
        fn message(&self, message: Message) -> pi_rs_chord::sync::SyncFuture<()> {
            let _ = self.0.send(message);
            Box::pin(async { Ok(()) })
        }
        fn closed(&self) {}
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let channel = Channel::start(
        b,
        Arc::new(Capture(tx)),
        ChannelOptions {
            codec: Codec::Binary,
            incarnation: [7; 16],
            calls: None,
            report: Arc::new(|_| {}),
        },
    )?;

    let fp = u64::from_str_radix(contract.fingerprint(), 16)
        .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?
        .to_be_bytes();
    channel.send(Message::new(Body::Hold {
        hold: 1,
        type_name: contract.name().into(),
        version: contract.version(),
        fingerprint: fp,
        params: Part::Text("{}".into()),
        address_kind: "server".into(),
        address_params: Box::new(empty()),
        principal: None,
        incarnation: None,
        chain: None,
        limit: None,
    }))?;

    let bound = deadline(async {
        while let Some(msg) = rx.recv().await {
            if let Body::Bound(entries) = msg.body {
                return Ok(entries);
            }
        }
        Err(pi_rs_chord::Error::Closed)
    })
    .await?;
    assert_eq!(bound.len(), 1);
    let mut members: Vec<(u64, JsonValue)> = bound[0]
        .members
        .iter()
        .map(|(i, p)| Ok((*i, p.parse(Limits::for_link(false))?)))
        .collect::<Result<_>>()?;
    members.sort_by_key(|(i, _)| *i);
    // Index 0 MUST be "alpha" ("A"), index 1 MUST be "zebra" ("Z").
    assert_eq!(
        members,
        vec![
            (0, JsonValue::String("A".into())),
            (1, JsonValue::String("Z".into())),
        ]
    );

    // Method index 0 MUST dispatch "a_call", not "z_call".
    channel.send(Message::new(Body::Call {
        call: 1,
        hold: 1,
        method: 0,
        args: Some(Payload {
            part: Part::Text("null".into()),
            segments: None,
        }),
    }))?;
    let result = deadline(async {
        while let Some(msg) = rx.recv().await {
            if let Body::Result {
                call: 1,
                value: Some(payload),
            } = msg.body
            {
                return payload.part.parse(Limits::for_link(false));
            }
        }
        Err(pi_rs_chord::Error::Closed)
    })
    .await?;
    assert_eq!(result, JsonValue::String("called_a".into()));

    channel.close();
    server.close();
    host.close(&Context::background()).await?;
    Ok(())
}
