use super::{HttpContribution, HttpResult};
use crate::{
    Result,
    host::HostError,
    registries::{RegistryConflict, RegistryProvider, RegistryRecord, define_registry},
    state::Batch,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Deref,
    sync::Arc,
};
use tokio::sync::watch;

#[derive(Clone)]
pub(crate) struct RegisteredHttp {
    value: Arc<HttpContribution>,
    retired: watch::Sender<bool>,
}
impl PartialEq for RegisteredHttp {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.value, &other.value)
    }
}
impl Deref for RegisteredHttp {
    type Target = HttpContribution;
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}
impl RegisteredHttp {
    pub(super) fn retire(&self) {
        self.retired.send_replace(true);
    }

    pub(super) fn retirement(&self) -> watch::Receiver<bool> {
        self.retired.subscribe()
    }
}
impl HttpContribution {
    pub(crate) fn registered(mut self, owner: &str) -> HttpResult<RegisteredHttp> {
        self.plugin = owner.into();
        self.validate()?;
        Ok(RegisteredHttp {
            value: Arc::new(self),
            retired: watch::channel(false).0,
        })
    }
}

#[derive(Clone, PartialEq)]
pub(crate) struct HttpTable(pub(super) Vec<RegisteredHttp>);
impl HttpTable {
    pub(crate) fn retire_missing(&self, next: Option<&Self>) {
        for entry in &self.0 {
            if next.is_none_or(|next| !next.0.contains(entry)) {
                entry.retire();
            }
        }
    }
}
pub(crate) type HttpRegistry = RegistryRecord<RegisteredHttp, HttpTable>;

pub(crate) fn new_registry(
    batch: Batch,
    report: Arc<dyn Fn(HostError) + Send + Sync>,
) -> Result<HttpRegistry> {
    Ok(RegistryRecord::new(
        define_registry("chord.http", 1)?,
        batch,
        move |event| {
            report(HostError {
                kind: "http_registry".into(),
                plugin: None,
                object: None,
                error: format!("{event:?}"),
            });
        },
    ))
}

pub(crate) fn registry_provider() -> RegistryProvider<RegisteredHttp, HttpTable> {
    RegistryProvider::<RegisteredHttp, HttpTable>::new(|table, ctx| {
        let mut routes: BTreeMap<_, &str> = BTreeMap::new();
        let mut sockets: BTreeMap<_, &str> = BTreeMap::new();
        let mut winner_patterns = BTreeSet::new();
        let mut report = |taken: &str, plugin: &str, message| {
            ctx.report(RegistryConflict {
                message,
                plugins: vec![taken.into(), plugin.into()],
            });
        };
        for entry in table {
            for (method, path, _) in &entry.value.routes {
                let key = (method.as_str(), shape(path));
                if let Some(taken) = routes.get(&key) {
                    report(
                        taken,
                        &entry.plugin,
                        format!("route {method} {path} is contributed twice"),
                    );
                } else {
                    routes.insert(key, entry.plugin.as_str());
                    winner_patterns.insert(format!("{method} {path}"));
                }
            }
            for path in entry.value.sockets.keys() {
                if let Some(taken) = sockets.get(&shape(path)) {
                    report(
                        taken,
                        &entry.plugin,
                        format!("socket {path} is contributed twice"),
                    );
                } else {
                    sockets.insert(shape(path), entry.plugin.as_str());
                }
            }
        }
        for entry in table {
            for wrap in &entry.value.wraps {
                if let Some(route) = &wrap.route
                    && !winner_patterns.contains(route)
                {
                    ctx.report(RegistryConflict {
                        message: format!("wrap {} names an unserved route {route}", wrap.name),
                        plugins: vec![entry.plugin.clone()],
                    });
                }
            }
        }
        Ok(HttpTable(
            table.iter().map(|entry| entry.value.clone()).collect(),
        ))
    })
}

fn shape(path: &str) -> Vec<&str> {
    path.split('/')
        .map(|part| if part.starts_with(':') { ":" } else { part })
        .collect()
}
