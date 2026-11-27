use super::*;
impl RemotePeer {
    pub(super) fn provider_message(&self, body: Body) -> Result<()> {
        match body {
            Body::Hold {
                hold,
                type_name,
                version,
                fingerprint: requested,
                params,
                principal,
                incarnation,
                address_kind,
                address_params,
                ..
            } => {
                if lock(&self.0.pending_routes).contains_key(&hold)
                    || lock(&self.0.serving).contains_key(&hold)
                {
                    return Err(Error::Invalid("hold id already in use".into()));
                }
                if matches!(params, Part::Deflated(_)) {
                    return Err(Error::Invalid("hold params must not be compressed".into()));
                }
                let Some(host) = self
                    .0
                    .host
                    .as_ref()
                    .and_then(crate::host::WeakHost::upgrade)
                else {
                    return Err(Error::Invalid("peer does not serve objects".into()));
                };
                let Some(contract) = host
                    .served_contract(&type_name)
                    .or_else(|| self.0.options.route_contracts.get(&type_name).cloned())
                else {
                    return self.send(Body::Failed {
                        hold,
                        cause: 2,
                        error: wire_error(
                            &ChordError::new("unavailable", "type is not served").into(),
                        )?,
                        retry_in: None,
                    });
                };
                if contract.kind() != crate::contracts::ContractKind::Remote {
                    return self.send(Body::Failed {
                        hold,
                        cause: 3,
                        error: wire_error(
                            &ChordError::new("forbidden", "local types never cross links").into(),
                        )?,
                        retry_in: None,
                    });
                }
                if contract.version() != version
                    || fingerprint(contract.fingerprint())? != requested
                {
                    return self.send(Body::Failed {
                        hold,
                        cause: 2,
                        error: wire_error(
                            &ChordError::new("contract_mismatch", "version is not served").into(),
                        )?,
                        retry_in: None,
                    });
                }
                let peer_id = self
                    .channel()?
                    .peer()
                    .map(|(id, _)| uuid::Uuid::from_bytes(id).to_string())
                    .unwrap_or_default();
                let caller = if self.0.options.role == LinkRole::Parent && self.0.options.trusted {
                    Caller {
                        principal: Some(
                            principal
                                .map(|p| p.to_serde())
                                .transpose()?
                                .unwrap_or_else(|| lock(&self.0.principal).clone()),
                        ),
                        incarnation: incarnation
                            .map(|i| uuid::Uuid::from_bytes(i).to_string())
                            .unwrap_or(peer_id),
                    }
                } else {
                    Caller {
                        principal: Some(lock(&self.0.principal).clone()),
                        incarnation: peer_id,
                    }
                };
                let (context, cancel) = Context::background().with_cancel();
                let parsed = params.parse(Limits::for_link(self.0.options.trusted))?;
                if let Err(error) =
                    crate::contracts::check_params(contract.params(), &parsed, "params")
                {
                    return self.fail_hold(hold, &error);
                }
                let local = {
                    let contract = contract.clone();
                    let parsed = parsed.clone();
                    let caller = caller.clone();
                    let context = context.clone();
                    move || {
                        host.hold_as(contract, parsed, caller, &context)
                            .map(Provided::Local)
                    }
                };
                if let Some(route) = self.0.options.route.clone() {
                    let principal = caller.principal.as_ref().ok_or(Error::Closed)?;
                    crate::data::principal_key(principal)?;
                    let key = Arc::new(());
                    let mut pending = lock(&self.0.pending_routes);
                    self.send(Body::Loading { hold })?;
                    pending.insert(
                        hold,
                        PendingRoute {
                            key: key.clone(),
                            principal: principal.clone(),
                            cancel: cancel.clone(),
                        },
                    );
                    drop(pending);
                    let peer = self.clone();
                    let request = RouteHold {
                        address_kind,
                        address_params: *address_params,
                        contract: contract.clone(),
                        params: parsed,
                        caller,
                        context: context.clone(),
                    };
                    // Don't await demand on the channel dispatcher: Release (or a drop)
                    // must be able to cancel it while the destination is starting.
                    tokio::spawn(async move {
                        let Ok(routed) = context
                            .await_with(async move { route(request).await })
                            .await
                        else {
                            return;
                        };
                        let mut pending = lock(&peer.0.pending_routes);
                        if !pending
                            .get(&hold)
                            .is_some_and(|route| Arc::ptr_eq(&route.key, &key))
                        {
                            return;
                        }
                        let held = match routed {
                            Ok(Some(remote))
                                if remote.0.contract.name() == contract.name()
                                    && remote.0.contract.version() == contract.version()
                                    && remote.0.contract.fingerprint()
                                        == contract.fingerprint() =>
                            {
                                Ok(Provided::Routed(remote))
                            }
                            Ok(Some(_)) => Err(ChordError::new(
                                "contract_mismatch",
                                "router returned a different contract",
                            )
                            .into()),
                            Ok(None) => local(),
                            Err(error) => Err(error),
                        };
                        let result = peer.finish_hold(hold, held, &context, cancel);
                        pending.remove(&hold);
                        drop(pending);
                        if let Err(error) = result {
                            (peer.0.options.report)(error.to_string());
                        }
                    });
                } else {
                    self.finish_hold(hold, local(), &context, cancel)?;
                }
            }
            Body::Release { hold } => {
                let pending = lock(&self.0.pending_routes).remove(&hold);
                if let Some(route) = pending {
                    route.cancel.cancel("released");
                }
                let entry = lock(&self.0.serving).remove(&hold);
                if let Some(entry) = entry {
                    entry.cancel.cancel("released");
                    entry.held.release();
                }
                let calls = lock(&self.0.incoming)
                    .iter()
                    .filter_map(|(id, c)| (c.hold == hold).then_some(*id))
                    .collect::<Vec<_>>();
                for call in calls {
                    if let Some(call) = lock(&self.0.incoming).remove(&call) {
                        call.cancel.cancel("released");
                    }
                }
            }
            Body::Call {
                call,
                hold,
                method,
                args,
            } => {
                if lock(&self.0.incoming).contains_key(&call) {
                    return Err(Error::Invalid("call id already in use".into()));
                }
                if !self.0.options.trusted
                    && lock(&self.0.incoming).len() >= self.0.options.call_window
                {
                    return Err(Error::Invalid("peer exceeded its call window".into()));
                }
                if lock(&self.0.pending_routes).contains_key(&hold) {
                    return self.send(Body::Error {
                        call,
                        error: wire_error(
                            &ChordError::new("not_ready", "hold has not bound").into(),
                        )?,
                    });
                }
                let held = {
                    let serving = lock(&self.0.serving);
                    let Some(entry) = serving.get(&hold) else {
                        return Ok(());
                    };
                    if entry.methods.is_empty() {
                        return self.send(Body::Error {
                            call,
                            error: wire_error(
                                &ChordError::new("not_ready", "hold has not bound").into(),
                            )?,
                        });
                    }
                    let name =
                        entry.methods.get(method as usize).cloned().ok_or_else(|| {
                            Error::Invalid("call has unknown method index".into())
                        })?;
                    (entry.held.clone(), name)
                };
                let contract = held.0.contract();
                let declaration =
                    contract.current().variants[&held.0.variant()?].methods[&held.1].clone();
                if self.0.options.role == LinkRole::Child
                    && args
                        .as_ref()
                        .is_some_and(|v| matches!(v.part, Part::Deflated(_)))
                {
                    return Err(Error::Invalid(
                        "child arguments must not be compressed".into(),
                    ));
                }
                let args = match payload::decode(args, &declaration.args, self.0.options.trusted) {
                    Ok(value) => value,
                    Err(error) => {
                        return self.send(Body::Error {
                            call,
                            error: wire_error(&error)?,
                        });
                    }
                };
                let (context, cancel) = Context::background().with_cancel();
                let key = Arc::new(());
                lock(&self.0.incoming).insert(
                    call,
                    Incoming {
                        key: key.clone(),
                        hold,
                        cancel,
                    },
                );
                let peer = self.clone();
                tokio::spawn(async move {
                    let result = held.0.call(&held.1, args, &context).await;
                    let reply = result
                        .and_then(|value| {
                            Ok(Body::Result {
                                call,
                                value: payload::encode(value, &declaration.result, true)?,
                            })
                        })
                        .or_else(|error| {
                            Ok::<Body, Error>(Body::Error {
                                call,
                                error: wire_error(&error)?,
                            })
                        });
                    let mut incoming = lock(&peer.0.incoming);
                    if !incoming
                        .get(&call)
                        .is_some_and(|entry| Arc::ptr_eq(&entry.key, &key))
                    {
                        return;
                    }
                    incoming.remove(&call);
                    match reply {
                        Ok(reply) => {
                            let _ = peer.send(reply);
                        }
                        Err(error) => (peer.0.options.report)(error.to_string()),
                    }
                });
            }
            Body::Abort { call } => {
                if let Some(entry) = lock(&self.0.incoming).remove(&call) {
                    entry.cancel.cancel("aborted by caller");
                }
            }
            Body::Retry { hold, hops } => {
                let held = lock(&self.0.serving).get(&hold).map(|s| s.held.clone());
                if let Some(held) = held {
                    match held {
                        Provided::Local(held) => held.retry()?,
                        Provided::Routed(held) if hops > 0 => held.retry_with_hops(hops - 1)?,
                        Provided::Routed(_) => {}
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn fail_hold(&self, hold: u64, error: &Error) -> Result<()> {
        self.send(Body::Failed {
            hold,
            cause: 1,
            error: wire_error(error)?,
            retry_in: None,
        })
    }
    fn finish_hold(
        &self,
        hold: u64,
        held: Result<Provided>,
        context: &Context,
        cancel: Cancel,
    ) -> Result<()> {
        let held = match held {
            Ok(held) => held,
            Err(error) => {
                cancel.cancel("hold failed");
                return self.fail_hold(hold, &error);
            }
        };
        let peer = self.clone();
        let key = Arc::new(());
        let observed = key.clone();
        let initial = Arc::new(Mutex::new(()));
        let subscribed = initial.clone();
        let _initial = lock(&initial);
        let observation = held.state().observe(
            context,
            move |call, _| {
                let _initial = lock(&subscribed);
                if let NativeWatchCall::Value { value } = call {
                    peer.provider_state(hold, &observed, value)?;
                }
                Ok(())
            },
            |_| {},
        )?;
        let state = observation.value.clone();
        lock(&self.0.serving).insert(
            hold,
            Serving {
                key: key.clone(),
                held,
                cancel,
                _observation: Some(observation),
                subscriptions: Vec::new(),
                events: Vec::new(),
                methods: Vec::new(),
            },
        );
        self.provider_state(hold, &key, state)
    }
    fn provider_state(&self, id: u64, key: &Arc<()>, state: RefState) -> Result<()> {
        let mut serving = lock(&self.0.serving);
        let Some(entry) = serving
            .get_mut(&id)
            .filter(|entry| Arc::ptr_eq(&entry.key, key))
        else {
            return Ok(());
        };
        entry.subscriptions.clear();
        entry.events.clear();
        entry.methods.clear();
        let held = &entry.held;
        match state {
            RefState::Loading => self.send(Body::Loading { hold: id }),
            RefState::Failed { cause, retry_at } => self.send(Body::Failed {
                hold: id,
                cause: match cause.kind {
                    FailureKind::Transient => 0,
                    FailureKind::Broken => 1,
                    FailureKind::Waiting => 2,
                    FailureKind::Denied => 3,
                },
                error: wire_error(&cause.error.into())?,
                retry_in: retry_at,
            }),
            RefState::Ended { error } => self.send(Body::Gone {
                hold: id,
                error: wire_error(&error.into())?,
            }),
            RefState::Live => {
                let variant = held.variant()?;
                let contract = held.contract();
                let names = contract.variant_member_names(Some(&variant))?;
                let variant = contract
                    .variant_order()
                    .iter()
                    .position(|n| n == &variant)
                    .ok_or_else(|| Error::Invalid("unknown live variant".into()))?;
                let mut members = Vec::new();
                let mut failed = Vec::new();
                let mut subscriptions = Vec::new();
                for (index, name) in names.state.iter().enumerate() {
                    let source = held.member(name)?;
                    match source.value() {
                        Ok(value) => members.push((
                            index as u64,
                            Part::origin(value.to_json().to_json()?, true)?,
                        )),
                        Err(error) => failed.push(FailedMember {
                            member: index as u64,
                            error: wire_error(&error)?,
                        }),
                    };
                    let peer = self.clone();
                    let encoder = Arc::new(Mutex::new(Encoder::default()));
                    let subscription = source.subscribe(
                        move |call, _| {
                            let mut entry = StateEntry {
                                hold: id,
                                members: Vec::new(),
                                failed: Vec::new(),
                                retired: Vec::new(),
                            };
                            match call {
                                WatchCall::Value { ops, retired, .. } => {
                                    let wire = lock(&encoder).encode(&ops)?;
                                    entry.members.push((
                                        index as u64,
                                        Part::origin(JsonValue::Array(wire).to_json()?, true)?,
                                    ));
                                    if retired {
                                        entry.retired.push(index as u64);
                                    }
                                }
                                WatchCall::Failed { error } => entry.failed.push(FailedMember {
                                    member: index as u64,
                                    error: wire_error(&Error::Invalid(error))?,
                                }),
                            }
                            peer.queue_state(entry)
                        },
                        |_| {},
                    );
                    subscriptions.push(subscription);
                }
                for (member, name) in names.events.iter().enumerate() {
                    let weak = Arc::downgrade(&self.0);
                    let listener = move |payload: JsonValue, at: u64| {
                        let Some(inner) = weak.upgrade() else { return };
                        let peer = RemotePeer(inner);
                        let result = (|| {
                            let value = Part::origin(payload.to_json()?, true)?;
                            {
                                let mut flow = lock(&peer.0.replica);
                                if flow.events_in_flight >= peer.0.options.event_window {
                                    return Ok(());
                                }
                                flow.events_in_flight += 1;
                            }
                            peer.send(Body::Event(vec![crate::link::WireEvent {
                                hold: id,
                                member: member as u64,
                                at,
                                value,
                            }]))
                        })();
                        if let Err(error) = result {
                            (peer.0.options.report)(error.to_string());
                        }
                    };
                    entry.events.push(match held {
                        Provided::Local(held) => held.subscribe_event_at(name, listener)?,
                        Provided::Routed(held) => held.subscribe_event_at(name, listener)?,
                    });
                }
                entry.methods = names.methods;
                entry.subscriptions = subscriptions;
                self.send_replica(Body::Bound(vec![BoundEntry {
                    hold: id,
                    variant: variant as u64,
                    members,
                    failed,
                }]))
            }
        }
    }
}
impl Provided {
    fn state(&self) -> NativeSource<RefState> {
        match self {
            Self::Local(held) => held.state(),
            Self::Routed(held) => held.state(),
        }
    }
    fn variant(&self) -> Result<String> {
        match self {
            Self::Local(held) => held.variant(),
            Self::Routed(held) => held
                .variant()
                .ok_or_else(|| ChordError::new("not_ready", "reference is not bound").into()),
        }
    }
    fn contract(&self) -> Arc<Contract> {
        match self {
            Self::Local(held) => held.contract(),
            Self::Routed(held) => held.0.contract.clone(),
        }
    }
    fn member(&self, name: &str) -> Result<Source> {
        match self {
            Self::Local(held) => held.member(name),
            Self::Routed(held) => held.member(name),
        }
    }
    async fn call(&self, method: &str, args: JsonValue, context: &Context) -> Result<JsonValue> {
        match self {
            Self::Local(held) => held.call(method, args, context).await,
            Self::Routed(held) => held.call(method, args, context).await,
        }
    }
    pub(super) fn release(&self) {
        match self {
            Self::Local(held) => held.release(),
            Self::Routed(held) => held.release(),
        }
    }
}
impl Host {
    pub fn accept(&self, link: Arc<dyn Link>, mut options: RemoteOptions) -> Result<RemotePeer> {
        options.role = LinkRole::Child;
        let peer = RemotePeer::serve(self.clone(), link, options)?;
        self.keep_peer(peer.clone());
        Ok(peer)
    }
    pub fn connect(&self, link: Arc<dyn Link>, mut options: RemoteOptions) -> Result<RemotePeer> {
        options.role = LinkRole::Parent;
        let peer = RemotePeer::serve(self.clone(), link, options)?;
        self.keep_peer(peer.clone());
        Ok(peer)
    }
}
