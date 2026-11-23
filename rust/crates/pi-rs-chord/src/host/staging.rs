use super::declarations::{RegistryDeclaration, Request};
use super::*;
impl Host {
    pub async fn load(&self, facets: Vec<PluginFacet>, context: &Context) -> Result<ReloadReport> {
        if lock(&self.0.state).loaded {
            return Err(Error::Invalid("host is already loaded".into()));
        }
        self.reload(facets, context).await
    }
    pub async fn reload(
        &self,
        facets: Vec<PluginFacet>,
        context: &Context,
    ) -> Result<ReloadReport> {
        self.reload_with_failures(facets, Vec::new(), context).await
    }
    pub async fn reload_with_failures(
        &self,
        facets: Vec<PluginFacet>,
        failures: Vec<PluginFailure>,
        context: &Context,
    ) -> Result<ReloadReport> {
        self.0.batch.assert_outside("host.reload")?;
        let _reload = self.0.reload.lock().await;
        context
            .check()
            .map_err(|e| ChordError::new("aborted", e.to_string()))?;
        let (old, objects) = {
            let state = lock(&self.0.state);
            if state.closed {
                return Err(Error::Closed);
            }
            (
                state.plugins.clone(),
                state.objects.values().cloned().collect::<Vec<_>>(),
            )
        };
        let mut names = BTreeSet::new();
        for facet in &facets {
            crate::contracts::check_name(&facet.name, "A plugin name")?;
            if !names.insert(facet.name.clone()) {
                return Err(Error::Invalid(format!("duplicate plugin {}", facet.name)));
            }
        }
        let mut failed: IndexMap<String, String> =
            failures.into_iter().map(|f| (f.plugin, f.error)).collect();
        let plugins = loop {
            let before = failed.len();
            let mut next = IndexMap::<String, Plugin>::new();
            // Held-back owners reserve their names before any new declaration is admitted.
            for (name, plugin) in &old {
                if failed.contains_key(name)
                    || facets
                        .iter()
                        .any(|f| f.name == *name && f.facet.same(&plugin.facet))
                {
                    next.insert(name.clone(), plugin.clone());
                }
            }
            for facet in &facets {
                if next.contains_key(&facet.name) || failed.contains_key(&facet.name) {
                    continue;
                }
                let mut setup = FacetHost {
                    plugin: facet.name.clone(),
                    http: None,
                    host: self.clone(),
                    types: IndexMap::new(),
                    registries: IndexMap::new(),
                    contributions: IndexMap::new(),
                };
                let built = (facet.facet.0)(&mut setup).and_then(|_| {
                    for plugin in next.values() {
                        if let Some(name) = setup
                            .types
                            .keys()
                            .find(|name| plugin.types.contains_key(*name))
                        {
                            return Err(Error::Invalid(format!("type {name} is already served")));
                        }
                        if let Some(name) = setup
                            .registries
                            .keys()
                            .find(|name| plugin.registries.contains_key(*name))
                        {
                            return Err(Error::Invalid(format!(
                                "registry {name} is already served"
                            )));
                        }
                    }
                    Ok(())
                });
                match built {
                    Ok(()) => {
                        next.insert(
                            facet.name.clone(),
                            Plugin {
                                facet: facet.facet.clone(),
                                http: setup.http,
                                types: setup.types,
                                registries: setup.registries,
                                contributions: setup.contributions,
                            },
                        );
                    }
                    Err(error) => {
                        failed.insert(facet.name.clone(), error.to_string());
                    }
                }
            }
            if failed.len() != before {
                continue;
            }
            let served = next
                .values()
                .flat_map(|p| p.types.iter().map(|(n, t)| (n.clone(), t.clone())))
                .collect::<IndexMap<_, _>>();
            let registries = next
                .values()
                .flat_map(|p| p.registries.iter().map(|(n, t)| (n.clone(), t.clone())))
                .collect::<IndexMap<_, _>>();
            for object in &objects {
                let previous = lock(&object.state).served.clone();
                let Some(previous) = previous else {
                    continue;
                };
                let Some(plugin) = next.get(&previous.plugin) else {
                    continue;
                };
                if old
                    .get(&previous.plugin)
                    .is_some_and(|p| p.facet.same(&plugin.facet))
                {
                    continue;
                }
                let Some(target) = plugin.types.get(object.contract.name()).cloned() else {
                    continue;
                };
                let mut recording =
                    ObjectSelf::new(self.clone(), object.clone(), target.clone(), true);
                let rehearsal =
                    (target.factory)(&object.params, object.principal.as_ref(), &mut recording)
                        .and_then(|_| {
                            validate_recorded(&lock(&object.state).recorded, &recording.recorded)
                        });
                if let Err(error) = rehearsal {
                    failed.insert(previous.plugin.clone(), error.to_string());
                    continue;
                }
                if !failed.is_empty() {
                    for request in &recording.recorded.requests {
                        let bindable = match request {
                            Request::Type(contract, params) => {
                                crate::contracts::check_params(
                                    contract.params(),
                                    params,
                                    "params",
                                )?;
                                served
                                    .get(contract.name())
                                    .is_some_and(|s| s.binds(contract))
                            }
                            Request::Registry(token) => registries
                                .get(&token.name)
                                .is_some_and(|d| d.token.version == token.version),
                        };
                        if !bindable {
                            failed.insert(
                                previous.plugin.clone(),
                                "request cannot bind against held-back declarations".into(),
                            );
                            break;
                        }
                    }
                }
            }
            if failed.len() == before {
                break next;
            }
        };
        let mut report = ReloadReport {
            failed: failed
                .into_iter()
                .map(|(plugin, error)| PluginFailure { plugin, error })
                .collect(),
            ..ReloadReport::default()
        };
        for (name, plugin) in &plugins {
            match old.get(name) {
                None => report.added.push(name.clone()),
                Some(was) if !was.facet.same(&plugin.facet) => report.changed.push(name.clone()),
                _ => {}
            }
        }
        for name in old.keys() {
            if !plugins.contains_key(name) {
                report.removed.push(name.clone());
            }
        }
        let served = plugins
            .values()
            .flat_map(|p| p.types.iter().map(|(n, t)| (n.clone(), t.clone())))
            .collect::<IndexMap<_, _>>();
        let declarations: IndexMap<String, RegistryDeclaration> = plugins
            .values()
            .flat_map(|p| p.registries.iter().map(|(n, d)| (n.clone(), d.clone())))
            .collect();
        let changed: BTreeSet<_> = report
            .changed
            .iter()
            .chain(report.removed.iter())
            .cloned()
            .collect();
        let mut restarting = Vec::new();
        let mut ending = Vec::new();
        let host = self.clone();
        let committed = plugins.clone();
        let types = served.clone();
        let active = objects.clone();
        let changes = changed.clone();
        self.0.batch.within(context, move || {
            {
                let mut state = lock(&host.0.state);
                state.loaded = true;
                state.plugins = committed;
                state.served = types.clone();
            }
            let mut registry_names = lock(&host.0.state)
                .registries
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>();
            registry_names.extend(declarations.keys().cloned());
            for name in registry_names {
                let declaration = declarations.get(&name);
                let token = declaration
                    .map(|d| d.token.clone())
                    .unwrap_or(crate::registries::define_registry(name.clone(), 1)?);
                let record = host.registry(token);
                let contributions = plugins
                    .iter()
                    .filter_map(|(owner, p)| {
                        p.contributions.get(&name).map(|(token, value)| {
                            (
                                crate::registries::Contributor::facet(owner),
                                token.version,
                                value.clone(),
                            )
                        })
                    })
                    .collect();
                if let Some(declaration) = declaration {
                    record.replace_versioned(
                        declaration.token.version,
                        declaration.provider.clone(),
                        contributions,
                        true,
                        &Context::background(),
                    )?;
                } else {
                    record.unserve(&Context::background())?;
                }
            }
            let previous_http = host.0.http.last_good();
            host.0.http.replace(
                crate::node::http::registry_provider(),
                plugins
                    .iter()
                    .filter_map(|(owner, plugin)| {
                        plugin
                            .http
                            .clone()
                            .map(|http| (crate::registries::Contributor::facet(owner), 1, http))
                    })
                    .collect(),
                &Context::background(),
            )?;
            if let Some(previous) = previous_http {
                previous.retire_missing(host.0.http.last_good().as_ref());
            }
            for object in active {
                let mut state = lock(&object.state);
                let affected = state
                    .served
                    .as_ref()
                    .is_none_or(|s| changes.contains(&s.plugin));
                if !affected {
                    continue;
                }
                let target = types.get(object.contract.name()).cloned();
                state.generation += 1;
                state.activating = false;
                state.served = target.clone();
                if target.is_some() {
                    state.old = state.attempt.take();
                    state.phase = RefState::Loading;
                } else {
                    state.phase = RefState::Ended {
                        error: ChordError::new("released", "type is no longer served"),
                    };
                }
                drop(state);
                object.moved()?;
            }
            Ok(())
        })?;
        for object in objects {
            let state = lock(&object.state);
            let affected = state
                .served
                .as_ref()
                .is_none_or(|s| changed.contains(&s.plugin))
                || matches!(state.phase, RefState::Loading) && state.attempt.is_none();
            if !affected {
                continue;
            }
            let kind = if state.served.is_none() {
                Fate::End
            } else if state.old.is_some() {
                Fate::Restart
            } else {
                Fate::Retry
            };
            drop(state);
            report.objects.push(ObjectFate {
                object: object.key.clone(),
                kind,
            });
            if kind == Fate::End {
                ending.push(object);
            } else {
                restarting.push(object);
            }
        }
        for object in ending {
            self.end_object(object, "type no longer served").await?;
        }
        for object in &restarting {
            self.activate(object.clone())?;
        }
        // Local singletons are eager. The temporary held reference is not a lifetime pin.
        for definition in served.values().filter(|s| s.singleton()) {
            let held = self.hold(
                definition.contract.clone(),
                JsonValue::Object(IndexMap::new()),
                context,
            )?;
            let object = held.object()?;
            if !restarting.iter().any(|o| Arc::ptr_eq(o, &object)) {
                restarting.push(object);
            }
            held.release();
        }
        for object in restarting {
            let mut moved = object.changed.subscribe();
            let ready = async {
                loop {
                    moved.borrow_and_update();
                    if !matches!(lock(&object.state).phase, RefState::Loading) {
                        return Ok::<(), Error>(());
                    }
                    moved.changed().await.map_err(|_| Error::Closed)?;
                }
            };
            context
                .await_with(tokio::time::timeout(self.0.options.ready_timeout, ready))
                .await
                .map_err(|e| ChordError::new("aborted", e.to_string()))?
                .map_err(|_| ChordError::new("unavailable", "host ready timeout"))??;
        }
        Ok(report)
    }
}
fn validate_recorded(old: &Recorded, next: &Recorded) -> Result<()> {
    for previous in &old.documents {
        if let Some(now) = next.documents.iter().find(|d| d.name() == previous.name()) {
            previous.compatible(now)?;
        }
    }
    for previous in &old.jobs {
        if let Some(now) = next.jobs.iter().find(|j| j.name == previous.name)
            && (now.version < previous.version
                || now.version == previous.version && now.fingerprint != previous.fingerprint)
        {
            return Err(Error::Invalid(format!(
                "{}: job schema changed without a version bump",
                now.name
            )));
        }
    }
    for previous in &old.tables {
        if let Some(now) = next.tables.iter().find(|t| t.name == previous.name)
            && (now.version < previous.version
                || now.version == previous.version && now.fingerprint != previous.fingerprint)
        {
            return Err(Error::Invalid(format!(
                "{}: table schema changed without a version bump",
                now.name
            )));
        }
    }
    Ok(())
}
