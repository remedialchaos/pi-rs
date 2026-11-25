//! Native equivalents of the p2 v1 plugin entry points.

pub mod p2_contracts;

use std::{
    collections::{BTreeMap, btree_map::Entry},
    sync::Arc,
};

use pi_rs_chord::sync::SyncFuture;

pub use pi_rs_chord::{
    Error, Result, context, contracts, data, documents,
    host::{
        Ctx, EventChannel, EventSubscription, Facet, FacetHost, Held, Host, Members, ObjectSelf,
        PluginFacet, ServeOptions, define_facet,
    },
    jobs, json, registries, state, store, watch,
};

pub const API_NAME: &str = "@p2/app";
pub const API_VERSION: &str = "v1";

pub(crate) type SessionAuthorizer =
    Arc<dyn Fn(Host, Principal, String, context::Context) -> SyncFuture<bool> + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FacetKind {
    Server,
    Worker,
    Web,
    Tui,
}

impl FacetKind {
    pub const ALL: [Self; 4] = [Self::Server, Self::Worker, Self::Web, Self::Tui];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Worker => "worker",
            Self::Web => "web",
            Self::Tui => "tui",
        }
    }
}

/// Native facets, ordered by plugin name. Each plugin may supply one facet per host kind.
#[derive(Clone, Default)]
pub struct PluginRegistry {
    plugins: BTreeMap<String, BTreeMap<FacetKind, Facet>>,
    session_authorizers: BTreeMap<String, SessionAuthorizer>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        name: impl Into<String>,
        kind: FacetKind,
        facet: Facet,
    ) -> Result<()> {
        let name = name.into();
        contracts::check_name(&name, "A plugin name")?;
        match self.plugins.entry(name.clone()).or_default().entry(kind) {
            Entry::Vacant(entry) => {
                entry.insert(facet);
                Ok(())
            }
            Entry::Occupied(_) => Err(Error::Invalid(format!(
                "duplicate p2 {} facet for {name}",
                kind.as_str()
            ))),
        }
    }

    pub fn select(&self, requested: Option<&[String]>) -> Result<Vec<String>> {
        if let Some(requested) = requested {
            for name in requested {
                if !self.plugins.contains_key(name) {
                    return Err(Error::Invalid(format!("Unknown p2 plugin: {name}")));
                }
            }
        }
        Ok(self
            .plugins
            .keys()
            .filter(|name| requested.is_none_or(|names| names.contains(name)))
            .cloned()
            .collect())
    }

    /// Register a server-side check for a guest's current access to a session.
    /// The callback receives the live server host and the cancellation context for this hold.
    pub fn register_session_authorizer(
        &mut self,
        plugin: impl Into<String>,
        authorize: impl Fn(Host, Principal, String, context::Context) -> SyncFuture<bool>
        + Send
        + Sync
        + 'static,
    ) -> Result<()> {
        let plugin = plugin.into();
        contracts::check_name(&plugin, "A plugin name")?;
        match self.session_authorizers.entry(plugin.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(Arc::new(authorize));
                self.plugins.entry(plugin).or_default();
                Ok(())
            }
            Entry::Occupied(_) => Err(Error::Invalid(format!(
                "duplicate p2 session authorizer for {plugin}"
            ))),
        }
    }

    pub(crate) fn session_authorizers(
        &self,
        selected: &[String],
    ) -> BTreeMap<String, SessionAuthorizer> {
        self.session_authorizers
            .iter()
            .filter(|(name, _)| selected.contains(name))
            .map(|(name, authorize)| (name.clone(), authorize.clone()))
            .collect()
    }

    pub fn facets(&self, kind: FacetKind, selected: &[String]) -> Vec<PluginFacet> {
        self.plugins
            .iter()
            .filter(|(name, _)| selected.contains(name))
            .filter_map(|(name, facets)| {
                facets
                    .get(&kind)
                    .map(|facet| PluginFacet::new(name, facet.clone()))
            })
            .collect()
    }
}

pub mod node {
    pub use crate::runtime::host_dirs::*;
}

pub mod server {
    pub use crate::hosts::session_workers::{
        SessionSuspendGuard, SessionWorkers, WORKER_INIT_PROVIDER_SERVICE, WorkerInitProvider,
    };
    pub use crate::runtime::{
        http::*,
        local_owner::*,
        server::{
            SESSION_ARCHIVED_REGISTRY, ServerInfo, ServerNatives, SessionArchivedContribution,
            SessionArchivedHandler, contribute_session_archived, install_service,
            run_session_archived, serve_session_archived_registry, server_contract, server_of,
            service_of,
        },
    };
    pub use pi_rs_chord::node::{http::*, workers::ProcessLock};
}

pub mod worker {
    pub use crate::hosts::worker::{
        WorkerInit, WorkerProcess, WorkerRegistries, durable, worker_of,
    };
    pub use crate::runtime::session_worker::*;
    pub use pi_rs_chord::node::workers::{Control, WorkerOptions, WorkerSession};
}

pub mod web {
    pub use pi_rs_chord::{surfaces, web};
}

pub mod tui {
    pub use pi_rs_chord::tui::*;
}

pub use crate::runtime::{
    address::{server_address, session_address},
    principals::{GrantMode, OwnerAccess, Principal},
};
