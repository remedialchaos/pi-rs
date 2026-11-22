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
                ..
            } => {
                if lock(&self.0.serving).contains_key(&hold) {
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
                let Some(contract) = host.served_contract(&type_name) else {
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
                let caller = if self.0.options.role == LinkRole::Parent {
                    Caller {
                        principal: Some(
                            principal
                                .map(|p| p.to_serde())
                                .transpose()?
                                .unwrap_or_else(|| self.0.options.principal.clone()),
                        ),
                        incarnation: incarnation
                            .map(|i| uuid::Uuid::from_bytes(i).to_string())
                            .unwrap_or(peer_id),
                    }
                } else {
                    Caller {
                        principal: Some(self.0.options.principal.clone()),
                        incarnation: peer_id,
                    }
                };
                let (context, cancel) = Context::background().with_cancel();
                let parsed = params.parse(Limits::for_link(self.0.options.trusted))?;
                let held = match host.hold_as(contract, parsed, caller, &context) {
                    Ok(held) => held,
                    Err(error) => {
                        return self.send(Body::Failed {
                            hold,
                            cause: 1,
                            error: wire_error(&error)?,
                            retry_in: None,
                        });
                    }
                };
                let peer = self.clone();
                let observation = held.state().observe(
                    &context,
                    move |call, _| {
                        if let NativeWatchCall::Value { value } = call {
                            peer.provider_state(hold, value)?;
                        }
                        Ok(())
                    },
                    |_| {},
                )?;
                let state = held.state().value();
                lock(&self.0.serving).insert(
                    hold,
                    Serving {
                        held,
                        cancel,
                        _observation: Some(observation),
                        subscriptions: Vec::new(),
                        methods: Vec::new(),
                    },
                );
                self.provider_state(hold, state)?;
            }
            Body::Release { hold } => {
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
                let args = if let Some(args) = args {
                    if self.0.options.role == LinkRole::Child
                        && matches!(args.part, Part::Deflated(_))
                    {
                        return Err(Error::Invalid(
                            "child arguments must not be compressed".into(),
                        ));
                    }
                    if args.segments.is_some() {
                        return self.send(Body::Error {
                            call,
                            error: wire_error(
                                &ChordError::invalid(
                                    "args",
                                    "JSON methods do not accept byte segments",
                                    None,
                                )
                                .into(),
                            )?,
                        });
                    }
                    args.part.parse(Limits::for_link(self.0.options.trusted))?
                } else {
                    JsonValue::Null
                };
                let (context, cancel) = Context::background().with_cancel();
                lock(&self.0.incoming).insert(call, Incoming { hold, cancel });
                let peer = self.clone();
                tokio::spawn(async move {
                    let result = held.0.call(&held.1, args, &context).await;
                    let active = lock(&peer.0.incoming).remove(&call).is_some();
                    if !active {
                        return;
                    }
                    let reply = result
                        .and_then(|value| {
                            if value == JsonValue::Null {
                                Ok(Body::Result { call, value: None })
                            } else {
                                Ok(Body::Result {
                                    call,
                                    value: Some(Payload {
                                        part: Part::origin(value.to_json()?, true)?,
                                        segments: None,
                                    }),
                                })
                            }
                        })
                        .or_else(|error| {
                            Ok::<Body, Error>(Body::Error {
                                call,
                                error: wire_error(&error)?,
                            })
                        });
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
            Body::Retry { hold, .. } => {
                let held = lock(&self.0.serving).get(&hold).map(|s| s.held.clone());
                if let Some(held) = held {
                    held.retry()?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn provider_state(&self, id: u64, state: RefState) -> Result<()> {
        let held = {
            let mut serving = lock(&self.0.serving);
            let Some(entry) = serving.get_mut(&id) else {
                return Ok(());
            };
            entry.subscriptions.clear();
            entry.held.clone()
        };
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
                let names = contract.member_names(Some(&variant))?;
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
                if let Some(entry) = lock(&self.0.serving).get_mut(&id) {
                    entry.methods = names.methods;
                    entry.subscriptions = subscriptions;
                }
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
