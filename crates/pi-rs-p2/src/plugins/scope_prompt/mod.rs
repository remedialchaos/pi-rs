//! `scope-prompt` built-in plugin: stages `<scope>` blocks per `requestId` for 60s
//! via `p2.scope-stage` and exposes them to the views send router.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
};
use serde::Deserialize;

use crate::api::v1::{
    FacetKind, PluginRegistry, Principal,
    p2_contracts::lookup,
    server::{install_service, service_of},
};

/// How long a staged scope block waits for its message before expiring.
pub const STAGED_FOR: Duration = Duration::from_secs(60);

/// Host-lifetime service key for reading & consuming staged scope blocks from the worker.
pub const SCOPE_STAGE_STORE_SERVICE: &str = "p2.scope-stage-store";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedScope {
    pub block: String,
    pub note: String,
    expires_at: Instant,
}

#[derive(Default)]
pub struct ScopeStageStore {
    staged: Mutex<HashMap<String, StagedScope>>,
}

impl ScopeStageStore {
    pub fn stage(&self, request_id: String, block: String, note: String) {
        self.stage_for(request_id, block, note, STAGED_FOR);
    }

    pub fn stage_for(&self, request_id: String, block: String, note: String, ttl: Duration) {
        let now = Instant::now();
        let mut guard = self.staged.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|_, entry| entry.expires_at > now);
        guard.insert(
            request_id,
            StagedScope {
                block,
                note,
                expires_at: now + ttl,
            },
        );
    }

    pub fn get(&self, request_id: &str) -> Option<(String, String)> {
        let now = Instant::now();
        let mut guard = self.staged.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|_, entry| entry.expires_at > now);
        guard
            .get(request_id)
            .map(|entry| (entry.block.clone(), entry.note.clone()))
    }

    pub fn drop_staged(&self, request_id: &str) -> bool {
        self.staged
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(request_id)
            .is_some()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StageArgs {
    request_id: String,
    block: String,
    note: String,
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let contract = Arc::new(lookup("p2.scope-stage")?);
    registry.register(
        "scope-prompt",
        FacetKind::Worker,
        define_facet(move |host| {
            let store = match service_of::<ScopeStageStore>(&host.host(), SCOPE_STAGE_STORE_SERVICE)
            {
                Ok(existing) => existing,
                Err(_) => {
                    let created = Arc::new(ScopeStageStore::default());
                    install_service(&host.host(), SCOPE_STAGE_STORE_SERVICE, &created)?;
                    created
                }
            };
            host.serve(
                contract.clone(),
                move |params, principal, _| {
                    let session = params.to_serde()?["session"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing session param".into()))?
                        .to_owned();
                    let parsed = principal
                        .and_then(|v| Principal::deserialize(v).ok())
                        .ok_or_else(|| {
                            ChordError::new(
                                "forbidden",
                                "only people who may send here stage a scope",
                            )
                        })?;
                    if !parsed.can_send(&session) {
                        return Err(ChordError::new(
                            "forbidden",
                            "only people who may send here stage a scope",
                        )
                        .into());
                    }
                    let store = store.clone();
                    let mut members = Members::variant("sender");
                    members.method("stage", move |args, _| {
                        let store = store.clone();
                        Box::pin(async move {
                            let parsed: StageArgs = serde_json::from_value(args.to_serde()?)?;
                            store.stage(parsed.request_id, parsed.block, parsed.note);
                            Ok(JsonValue::Null)
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )
}
