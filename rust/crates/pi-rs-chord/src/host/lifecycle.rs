use super::*;
use crate::context::AbortSignal;
impl Host {
    pub(super) fn activate(&self, object: Arc<Object>) -> Result<()> {
        {
            let mut state = lock(&object.state);
            if state.activating || matches!(state.phase, RefState::Live | RefState::Ended { .. }) {
                return Ok(());
            }
            state.activating = true;
        }
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|e| Error::Invalid(e.to_string()))?;
        let host = self.clone();
        runtime.spawn(async move {
            let generation = lock(&object.state).generation;
            if let Err(error) = host.run_activation(object.clone(), generation).await {
                let error = as_chord(error);
                let retry = {
                    let mut state = lock(&object.state);
                    if state.generation != generation
                        || matches!(state.phase, RefState::Ended { .. })
                    {
                        return;
                    }
                    state.activating = false;
                    let retry = error.code != "invalid"
                        && error.code != "forbidden"
                        && error.code != "internal";
                    state.delay = Some(next_delay(state.delay));
                    state.phase = RefState::Failed {
                        cause: FailureCause {
                            kind: if error.code == "forbidden" {
                                FailureKind::Denied
                            } else if retry {
                                FailureKind::Transient
                            } else {
                                FailureKind::Broken
                            },
                            error: error.clone(),
                        },
                        retry_at: if retry { state.delay } else { None },
                    };
                    retry
                };
                let _ = object.moved();
                (host.0.options.on_error)(HostError {
                    kind: "attempt_failed".into(),
                    plugin: lock(&object.state)
                        .served
                        .as_ref()
                        .map(|s| s.plugin.clone()),
                    object: Some(object.key.clone()),
                    error: error.to_string(),
                });
                if retry {
                    let delay = lock(&object.state).delay.unwrap_or(1000);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    if lock(&object.state).generation == generation {
                        let _ = host.activate(object);
                    }
                }
            }
        });
        Ok(())
    }
    fn run_activation(&self, object: Arc<Object>, generation: u64) -> SyncFuture<()> {
        let host = self.clone();
        Box::pin(async move {
            let _serial = object.serial.lock().await;
            let old = lock(&object.state).old.clone();
            if let Some(old) = old {
                host.0
                    .cycles
                    .set_edges(object.key.clone(), vec![old.id.clone()]);
                host.drain(
                    object.clone(),
                    old,
                    "restart",
                    host.0.options.reload_drain_timeout,
                )
                .await?;
                lock(&object.state).old.take();
            }
            let served = lock(&object.state)
                .served
                .clone()
                .ok_or_else(|| ChordError::new("unavailable", "no provider serves this object"))?;
            let (context, cancel) = host.0.context.with_cancel();
            let mut declaration =
                ObjectSelf::new(host.clone(), object.clone(), served.clone(), false);
            if let Some(admit) = &served.options.admit {
                let principal = object.principal.as_ref().ok_or_else(|| {
                    ChordError::new("forbidden", "admission requires a principal")
                })?;
                let source = admit(&object.params, principal)?;
                let decision = source.current()?;
                let weak = Arc::downgrade(&object);
                let changed = host.clone();
                let end_object = Arc::downgrade(&object);
                let ended = host.clone();
                let observation = source.observe(
                    &host.0.context,
                    move |_, _| {
                        if let Some(object) = weak.upgrade() {
                            changed.admission_changed(object)?;
                        }
                        Ok(())
                    },
                    move |_| {
                        if let Some(object) = end_object.upgrade() {
                            let _ = ended.admission_changed(object);
                        }
                    },
                )?;
                *lock(&object.admission) = Some(observation);
                match decision {
                    Admission::Allow { value } => declaration.admitted = Some(value),
                    Admission::Deny { reason } => {
                        return Err(ChordError::new("forbidden", reason).into());
                    }
                }
            }
            let members =
                (served.factory)(&object.params, object.principal.as_ref(), &mut declaration)
                    .map_err(|e| match e {
                        Error::Chord(_) => e,
                        other => ChordError::new("internal", other.to_string()).into(),
                    })?;
            let attempt = declaration
                .attempt
                .take()
                .ok_or_else(|| Error::Invalid("activation has no document attempt".into()))?;
            let caller = Caller {
                principal: object
                    .principal
                    .clone()
                    .or_else(|| host.0.options.principal.clone()),
                incarnation: host.incarnation().into(),
            };
            let opening = Ctx::new(&host, context.clone(), caller, Vec::new());
            let preparation = async {
                let edges = declaration
                    .dependencies
                    .iter()
                    .filter_map(|d| d.object().ok().map(|o| o.key.clone()))
                    .collect::<Vec<_>>();
                host.0.cycles.set_edges(object.key.clone(), edges);
                for dependency in &declaration.dependencies {
                    if Arc::ptr_eq(&dependency.object()?, &object) {
                        return Err(ChordError::new("invalid", "object depends on itself").into());
                    }
                    dependency.wait(&context).await?;
                }
                let data_load = host.0.host_data.read().await;
                if let Some(store) = &host.0.options.store
                    && !declaration.recorded.tables.is_empty()
                {
                    let tables = declaration
                        .recorded
                        .tables
                        .iter()
                        .cloned()
                        .map(crate::store::TableHistory::base)
                        .collect::<Vec<_>>();
                    store
                        .prepare_tables(served.contract.name(), &tables, &context)
                        .await?;
                }
                attempt.activate(&context).await?;
                drop(data_load);
                for opener in std::mem::take(&mut declaration.openers) {
                    context
                        .await_with(opener(opening.clone()))
                        .await
                        .map_err(|e| ChordError::new("aborted", e.to_string()))??;
                }
                validate_members(&served.contract, &members)?;
                let mut jobs = Vec::new();
                for pending in std::mem::take(&mut declaration.jobs) {
                    let store = if pending.definition.persist.is_some() {
                        let mut shared = lock(&object.job_store);
                        if shared.is_none() {
                            let engine = host
                                .0
                                .options
                                .store
                                .clone()
                                .ok_or_else(|| Error::Invalid("job store is absent".into()))?;
                            *shared =
                                Some(crate::jobs::JobStore::new(engine, served.contract.name())?);
                        }
                        shared.clone()
                    } else {
                        None
                    };
                    let job = crate::jobs::Job::new(
                        pending.definition,
                        pending.implementation,
                        host.batch(),
                        store,
                    )
                    .await?;
                    *lock(&pending.binding.0) = Some(job.clone());
                    jobs.push(job);
                }
                Ok(jobs)
            }
            .await;
            opening.settle();
            let jobs = match preparation {
                Ok(jobs) => jobs,
                Err(error) => {
                    cancel.cancel("activation failed");
                    object.documents.fence(attempt.token());
                    for dependency in declaration.dependencies {
                        dependency.release();
                    }
                    for cleanup in declaration.cleanups.into_iter().rev() {
                        let _ = tokio::time::timeout(
                            host.0.options.close_timeout,
                            cleanup("activation failed".into()),
                        )
                        .await;
                    }
                    return Err(error);
                }
            };
            let (changed, _) = watch::channel(0);
            let instance = Arc::new(Instance {
                id: uuid_v7()?,
                served: served.clone(),
                members,
                context: context.clone(),
                cancel,
                calls: AtomicUsize::new(0),
                changed,
                dependencies: Mutex::new(declaration.dependencies),
                jobs,
                doc_attempt: attempt,
                cleanups: Mutex::new(declaration.cleanups),
                recorded: declaration.recorded.clone(),
                opens_natives: declaration.opens_natives,
            });
            let mut publish = false;
            host.batch().commit(&context, || {
                let kept = {
                    let mut state = lock(&object.state);
                    if state.generation != generation
                        || matches!(state.phase, RefState::Ended { .. })
                    {
                        return Ok(());
                    }
                    state.recorded = instance.recorded.clone();
                    state.attempt = Some(instance.clone());
                    state.activating = false;
                    state.delay = None;
                    state.phase = RefState::Live;
                    state.kept_contributions.take()
                };
                if let Some(kept) = kept {
                    for token in kept.tokens {
                        host.registry(token).remove_owner(&kept.owner, &context)?;
                    }
                }
                let owner = crate::registries::Contributor::object(
                    &instance.served.plugin,
                    instance.served.contract.name(),
                );
                // Remove the retained attempt and add its successor in one frame,
                // before observers can see the successor live (upstream F-c).
                for (token, value) in declaration.contributions {
                    host.registry(token.clone()).contribute(
                        owner.clone(),
                        token.version,
                        value,
                        &context,
                    )?;
                }
                object.moved()?;
                publish = true;
                Ok(())
            })?;
            if !publish {
                host.drain(object.clone(), instance, "superseded", Duration::ZERO)
                    .await?;
                return Ok(());
            }
            for job in &instance.jobs {
                job.live()?;
            }
            host.0.cycles.remove_edges(&object.key);
            host.publish_activity()?;
            for routine in declaration.routines {
                let host = host.clone();
                let object = object.clone();
                let instance = instance.clone();
                tokio::spawn(async move {
                    let mut delay = None;
                    loop {
                        let ctx = Ctx::new(
                            &host,
                            instance.context.clone(),
                            Caller {
                                principal: object
                                    .principal
                                    .clone()
                                    .or_else(|| host.0.options.principal.clone()),
                                incarnation: host.incarnation().into(),
                            },
                            vec![instance.id.clone()],
                        );
                        let result = ctx.context.await_with(routine(ctx.clone())).await;
                        ctx.settle();
                        match result {
                            Ok(Ok(())) | Err(_) => break,
                            Ok(Err(error)) => {
                                (host.0.options.on_error)(HostError {
                                    kind: "routine_failed".into(),
                                    plugin: Some(instance.served.plugin.clone()),
                                    object: Some(object.key.clone()),
                                    error: error.to_string(),
                                });
                                let wait = next_delay(delay);
                                delay = Some(wait);
                                if instance
                                    .context
                                    .await_with(tokio::time::sleep(Duration::from_millis(wait)))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
            Ok(())
        })
    }
    fn admission_changed(&self, object: Arc<Object>) -> Result<()> {
        let previous = {
            let mut state = lock(&object.state);
            if matches!(state.phase, RefState::Ended { .. }) {
                return Ok(());
            }
            state.generation += 1;
            state.activating = false;
            state.phase = RefState::Loading;
            state.old = state.attempt.take();
            state.old.clone()
        };
        if let Some(instance) = previous {
            instance.cancel.cancel("admission changed");
        }
        object.moved()?;
        self.activate(object)
    }
    fn drain(
        &self,
        object: Arc<Object>,
        instance: Arc<Instance>,
        reason: &str,
        bound: Duration,
    ) -> SyncFuture<()> {
        let host = self.clone();
        let reason = reason.to_string();
        Box::pin(async move {
            let mut changes = instance.changed.subscribe();
            let drained = async {
                loop {
                    changes.borrow_and_update();
                    if instance.calls.load(Ordering::Acquire) == 0 {
                        break;
                    }
                    if changes.changed().await.is_err() {
                        break;
                    }
                }
            };
            let _ = tokio::time::timeout(bound, drained).await;
            instance.cancel.cancel(&reason);
            object.documents.fence(instance.doc_attempt.token());
            for job in &instance.jobs {
                job.close(&reason).await?;
            }
            for dependency in std::mem::take(&mut *lock(&instance.dependencies)) {
                dependency.release();
            }
            let owner = crate::registries::Contributor::object(
                &instance.served.plugin,
                instance.served.contract.name(),
            );
            let keep = {
                let mut state = lock(&object.state);
                // Native-owned contributions leave before closing their resources
                // (F-b). Pure contributions remain until a successor goes live.
                let keep = reason == "restart"
                    && !instance.opens_natives
                    && state.served.is_some()
                    && !matches!(state.phase, RefState::Ended { .. });
                if keep {
                    state.kept_contributions = Some(KeptContributions {
                        owner: owner.clone(),
                        tokens: instance.recorded.contributions.clone(),
                    });
                }
                keep
            };
            if !keep {
                for token in &instance.recorded.contributions {
                    host.registry(token.clone())
                        .remove_owner(&owner, &Context::background())?;
                }
            }
            let cleanups = std::mem::take(&mut *lock(&instance.cleanups));
            for cleanup in cleanups.into_iter().rev() {
                match tokio::time::timeout(host.0.options.close_timeout, cleanup(reason.clone()))
                    .await
                {
                    Ok(Ok(())) => {}
                    other => (host.0.options.on_error)(HostError {
                        kind: "cleanup_failed".into(),
                        plugin: Some(instance.served.plugin.clone()),
                        object: Some(object.key.clone()),
                        error: format!("{other:?}"),
                    }),
                }
            }
            // Member readables may be shared by factories. Release this attempt's
            // subscriptions and scoped follow resources, not the borrowed sources.
            host.publish_activity()?;
            Ok(())
        })
    }
    pub(super) fn unheld(&self, object: Arc<Object>) {
        let state = lock(&object.state);
        if state.holds != 0
            || state.served.as_ref().is_some_and(|s| s.singleton())
            || state
                .attempt
                .as_ref()
                .is_some_and(|i| !i.jobs.iter().all(|j| j.running().is_empty()))
        {
            return;
        }
        let linger = state.linger;
        let wait = state
            .served
            .as_ref()
            .map_or(Duration::ZERO, |s| s.options.linger);
        drop(state);
        let host = self.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                tokio::time::sleep(wait).await;
                let should_end = {
                    let state = lock(&object.state);
                    state.holds == 0 && state.linger == linger
                };
                if should_end {
                    let _ = host.end_object(object, "unheld").await;
                }
            });
        }
    }
    pub(super) fn end_object(&self, object: Arc<Object>, reason: &str) -> SyncFuture<()> {
        self.end_object_with(object, ChordError::new("released", reason))
    }

    fn end_object_with(&self, object: Arc<Object>, error: ChordError) -> SyncFuture<()> {
        let host = self.clone();
        let reason = error.message.clone();
        let instances = {
            let mut state = lock(&object.state);
            if matches!(state.phase, RefState::Ended { .. }) {
                return Box::pin(async { Ok(()) });
            }
            state.generation += 1;
            state.activating = false;
            state.phase = RefState::Ended { error };
            state
                .attempt
                .take()
                .into_iter()
                .chain(state.old.take())
                .collect::<Vec<_>>()
        };
        {
            let mut state = lock(&host.0.state);
            if state
                .objects
                .get(&object.key)
                .is_some_and(|o| Arc::ptr_eq(o, &object))
            {
                host.0.cycles.remove_edges(&object.key);
                state.objects.shift_remove(&object.key);
            }
        }
        let moved = object.moved();
        lock(&object.admission).take();
        Box::pin(async move {
            moved?;
            for instance in instances {
                host.drain(
                    object.clone(),
                    instance,
                    &reason,
                    host.0.options.close_timeout,
                )
                .await?;
            }
            let kept = lock(&object.state).kept_contributions.take();
            if let Some(kept) = kept {
                host.batch().commit(&Context::background(), || {
                    for token in kept.tokens {
                        host.registry(token)
                            .remove_owner(&kept.owner, &Context::background())?;
                    }
                    Ok(())
                })?;
            }
            object.documents.end();
            lock(&object.job_store).take();
            host.publish_presence(object.contract.name())?;
            host.publish_activity()?;
            Ok(())
        })
    }
    pub async fn revoke(&self, principal: &serde_json::Value, context: &Context) -> Result<()> {
        context
            .check()
            .map_err(|e| ChordError::new("aborted", e.to_string()))?;
        if let Some(policy) = self.0.options.principal_policy {
            let own = self
                .0
                .options
                .principal
                .as_ref()
                .map(policy.key)
                .transpose()?;
            let matches = |who: &serde_json::Value| {
                (policy.revoked_by)(who, principal)
                    && (policy.key)(who).is_ok_and(|key| own.as_ref() != Some(&key))
            };
            let (objects, links) = {
                let state = lock(&self.0.state);
                let objects = state
                    .objects
                    .values()
                    .filter(|object| object.principal.as_ref().is_some_and(matches))
                    .cloned()
                    .collect::<Vec<_>>();
                (objects, state.links.clone())
            };
            let drains: Vec<_> = objects
                .into_iter()
                .map(|object| self.end_object_with(object, revoked()))
                .collect();
            for link in &links {
                link.revoke_forwarded(principal, matches)?;
            }
            for link in links {
                link.close_revoked_child(matches);
            }
            for drain in drains {
                drain.await?;
            }
            return Ok(());
        }
        let key = crate::data::principal_key(principal)?;
        let objects = {
            let mut state = lock(&self.0.state);
            state.revoked.insert(key.clone());
            state
                .objects
                .values()
                .filter(|o| {
                    o.principal
                        .as_ref()
                        .is_some_and(|p| crate::data::principal_key(p).is_ok_and(|k| k == key))
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        for object in objects {
            self.end_object(object, "principal revoked").await?;
        }
        Ok(())
    }
    pub async fn close(&self, context: &Context) -> Result<()> {
        let objects = {
            let mut state = lock(&self.0.state);
            if state.closed {
                return Ok(());
            }
            state.closed = true;
            state.objects.values().cloned().collect::<Vec<_>>()
        };
        self.0.cancel.cancel("host closed");
        if let Some(http) = self.0.http.last_good() {
            http.retire_missing(None);
        }
        self.0.http.close();
        let links = std::mem::take(&mut lock(&self.0.state).links);
        for link in links {
            link.close();
        }
        for object in objects {
            context
                .await_with(self.end_object(object, "host closed"))
                .await
                .map_err(|e| ChordError::new("aborted", e.to_string()))??;
        }
        for registry in lock(&self.0.state).registries.values() {
            registry.close();
        }
        Ok(())
    }
    pub fn served_contract(&self, name: &str) -> Option<Arc<Contract>> {
        lock(&self.0.state)
            .served
            .get(name)
            .map(|s| s.contract.clone())
    }
}
struct Calling {
    host: Host,
    instance: Arc<Instance>,
    ctx: Option<Ctx>,
}
impl Drop for Calling {
    fn drop(&mut self) {
        if let Some(ctx) = &self.ctx {
            ctx.settle();
        }
        let remaining = self
            .instance
            .calls
            .fetch_sub(1, Ordering::AcqRel)
            .saturating_sub(1);
        self.instance.changed.send_replace(remaining);
        let _ = self.host.publish_activity();
    }
}
impl Held {
    pub async fn call(
        &self,
        method: &str,
        args: JsonValue,
        context: &Context,
    ) -> Result<JsonValue> {
        let object = self.object()?;
        let host = Host(object.host.upgrade().ok_or(Error::Closed)?);
        let ctx = Ctx::new(&host, context.clone(), self.0.caller.clone(), Vec::new());
        self.call_with_chain(method, args, &ctx).await
    }
    pub(super) async fn call_with_chain(
        &self,
        method: &str,
        args: JsonValue,
        up: &Ctx,
    ) -> Result<JsonValue> {
        let object = self.object()?;
        let host = Host(object.host.upgrade().ok_or(Error::Closed)?);
        let instance = loop {
            let selected = {
                let state = lock(&object.state);
                let selected = if matches!(state.phase, RefState::Live) {
                    state
                        .attempt
                        .clone()
                        .filter(|i| i.served.binds(&self.0.contract))
                } else if !up.old_chain.is_empty() {
                    state
                        .old
                        .clone()
                        .filter(|i| i.served.binds(&self.0.contract))
                } else {
                    None
                };
                if let Some(instance) = &selected {
                    instance.calls.fetch_add(1, Ordering::AcqRel);
                }
                selected
            };
            if let Some(instance) = selected {
                break instance;
            }
            let cycle = host
                .0
                .cycles
                .watch(object.key.clone(), up.old_chain.clone());
            tokio::select! {result=self.wait(&up.context)=>result?,error=cycle.cut()=>return Err(error.into())}
        };
        let mut calling = Calling {
            host: host.clone(),
            instance: instance.clone(),
            ctx: None,
        };
        let method_fn = instance
            .members
            .methods
            .get(method)
            .cloned()
            .ok_or_else(|| {
                ChordError::invalid("member", format!("unknown method {method}"), None)
            })?;
        let variant = instance
            .served
            .contract
            .current()
            .variants
            .get(&instance.members.variant)
            .ok_or_else(|| Error::Invalid("member variant is absent".into()))?;
        let schema = variant
            .methods
            .get(method)
            .ok_or_else(|| ChordError::invalid("member", "method is not declared", None))?;
        crate::data::Validator::compile(&schema.args.schema.to_serde()?)?
            .check(&args.to_serde()?, "args")?;
        let signals = up
            .context
            .abort_signal()
            .into_iter()
            .chain(instance.context.abort_signal());
        let context = up.context.with_abort_signal(AbortSignal::any(signals));
        let mut chain = up.old_chain.clone();
        chain.push(instance.id.clone());
        let ctx = Ctx::new(
            &host,
            context.clone(),
            Caller {
                principal: up.principal.clone(),
                incarnation: up.incarnation.clone(),
            },
            chain,
        );
        calling.ctx = Some(ctx.clone());
        host.publish_activity()?;
        let result = context
            .await_with(method_fn(args, ctx))
            .await
            .map_err(|e| ChordError::new("aborted", e.to_string()))??;
        crate::data::Validator::compile(&schema.result.schema.to_serde()?)?
            .check(&result.to_serde()?, "result")?;
        Ok(result)
    }
}
fn validate_members(contract: &Contract, members: &Members) -> Result<()> {
    let variant = contract
        .current()
        .variants
        .get(&members.variant)
        .ok_or_else(|| {
            Error::Invalid(format!(
                "{}: undeclared variant {}",
                contract.name(),
                members.variant
            ))
        })?;
    if variant.state.len() != members.state.len()
        || variant.methods.len() != members.methods.len()
        || variant.state.keys().any(|n| !members.state.contains_key(n))
        || variant
            .methods
            .keys()
            .any(|n| !members.methods.contains_key(n))
    {
        return Err(ChordError::new(
            "invalid",
            format!("{}: factory members differ from contract", contract.name()),
        )
        .into());
    }
    for (name, schema) in &variant.state {
        let value = members.state[name].value()?.to_json();
        crate::data::Validator::compile(&schema.schema.to_serde()?)?
            .check(&value.to_serde()?, name)?;
    }
    Ok(())
}
