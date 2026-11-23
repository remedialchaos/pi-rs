//! The session worker's start parameters and server-only lifecycle contract.

use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::{Address, Contract, Member, schema as s},
    data::{ChordError, join_key},
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
    sync::SyncFuture,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub cwd: String,
    pub env: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInit {
    pub id: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    pub model: ModelRef,
    pub thinking_level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<bool>,
}

impl SessionInit {
    pub fn location(&self) -> Location {
        Location {
            cwd: self.cwd.clone(),
            env: self.env.clone(),
        }
    }
}

pub fn worker_boundary_contract() -> Result<Arc<Contract>> {
    let location = s::obj([
        ("cwd", s::required(s::str())),
        ("env", s::required(s::union([s::str(), s::nul()]))),
    ]);
    Ok(Arc::new(Contract::remote(
        "p2.worker-boundary",
        2,
        vec![("session".into(), s::str())],
        IndexMap::from([(
            "server".into(),
            IndexMap::from([
                (
                    "prepare".into(),
                    Member::Method {
                        args: s::union([s::nul(), s::obj([("to", s::required(location.clone()))])]),
                        result: s::obj([
                            ("accepted", s::required(s::bool())),
                            ("tasks", s::required(s::num())),
                            ("busy", s::required(s::arr(s::num()))),
                            ("refusal", s::opt(s::str())),
                        ]),
                    },
                ),
                (
                    "cancel".into(),
                    Member::Method {
                        args: s::nul(),
                        result: s::nul(),
                    },
                ),
                (
                    "relocate".into(),
                    Member::Method {
                        args: location,
                        result: s::nul(),
                    },
                ),
            ]),
        )]),
        Arc::new(|params| {
            let value = params.to_serde()?;
            let id = value
                .get("session")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Invalid("missing session".into()))?;
            Ok(Address {
                kind: "session".into(),
                params: JsonValue::from_serde(json!({"id":id})),
                key: join_key(&["session".into(), id.into()]),
            })
        }),
    )?))
}

#[derive(Clone, Default)]
pub struct Liveness {
    activity: Arc<Mutex<Activity>>,
}

#[derive(Default)]
struct Activity {
    active_calls: usize,
    relocating: bool,
}

struct CallGuard(Liveness);
impl Drop for CallGuard {
    fn drop(&mut self) {
        self.0
            .activity
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .active_calls -= 1;
    }
}

impl Liveness {
    pub async fn call<T>(&self, work: impl Future<Output = Result<T>>) -> Result<T> {
        {
            let mut activity = self.activity.lock().unwrap_or_else(|err| err.into_inner());
            if activity.relocating {
                return Err(Error::Invalid(
                    "Working directory is changing; wait before sending another message".into(),
                ));
            }
            activity.active_calls += 1;
        }
        let _guard = CallGuard(self.clone());
        work.await
    }

    fn freeze(&self) -> bool {
        let mut activity = self.activity.lock().unwrap_or_else(|err| err.into_inner());
        if activity.relocating || activity.active_calls > 0 {
            return false;
        }
        activity.relocating = true;
        true
    }

    fn unfreeze(&self) {
        self.activity
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .relocating = false;
    }

    pub fn relocating(&self) -> bool {
        self.activity
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .relocating
    }
}

/// Inspection supplied by the session runtime under the admissions fence.
pub struct WorkerInspection {
    pub tasks: u64,
    pub busy: Vec<u64>,
}

pub trait WorkerBoundaryBackend: Send + Sync + 'static {
    fn inspect(&self, context: Context) -> SyncFuture<WorkerInspection>;
    fn check_relocation(
        &self,
        from: Location,
        to: Location,
        context: Context,
    ) -> SyncFuture<Option<String>>;
    fn relocate(&self, location: Location, context: Context) -> SyncFuture<()>;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PreparedWorker {
    pub accepted: bool,
    pub tasks: u64,
    pub busy: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

struct PrepareFence<'a> {
    liveness: &'a Liveness,
    release: bool,
}

impl Drop for PrepareFence<'_> {
    fn drop(&mut self) {
        if self.release {
            self.liveness.unfreeze();
        }
    }
}

pub struct WorkerBoundary {
    liveness: Liveness,
    location: watch::Sender<(u64, Location)>,
    backend: Arc<dyn WorkerBoundaryBackend>,
    lifecycle: tokio::sync::Mutex<()>,
}

impl WorkerBoundary {
    pub fn new(location: Location, backend: Arc<dyn WorkerBoundaryBackend>) -> Self {
        Self {
            liveness: Liveness::default(),
            location: watch::channel((0, location)).0,
            backend,
            lifecycle: tokio::sync::Mutex::new(()),
        }
    }

    pub fn liveness(&self) -> &Liveness {
        &self.liveness
    }

    pub fn location(&self) -> watch::Receiver<(u64, Location)> {
        self.location.subscribe()
    }

    pub async fn prepare(&self, to: Option<Location>, context: Context) -> Result<PreparedWorker> {
        let _lifecycle = self.lifecycle.lock().await;
        let frozen = self.liveness.freeze();
        let mut fence = PrepareFence {
            liveness: &self.liveness,
            release: frozen,
        };
        let WorkerInspection { tasks, mut busy } = self.backend.inspect(context.clone()).await?;
        busy.sort_unstable();
        busy.dedup();
        let refusal = match to {
            Some(to) if frozen && tasks == 0 => {
                let from = self.location.borrow().1.clone();
                self.backend.check_relocation(from, to, context).await?
            }
            _ => None,
        };
        let accepted = frozen && tasks == 0 && refusal.is_none();
        fence.release = frozen && !accepted;
        Ok(PreparedWorker {
            accepted,
            tasks,
            busy,
            refusal,
        })
    }

    pub async fn cancel(&self) {
        let _lifecycle = self.lifecycle.lock().await;
        self.liveness.unfreeze();
    }

    pub async fn relocate(&self, location: Location, context: Context) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        if !self.liveness.relocating() {
            return Err(Error::Invalid("Relocation was not prepared".into()));
        }
        self.backend.relocate(location.clone(), context).await?;
        self.location.send_modify(|(version, current)| {
            *version += 1;
            *current = location;
        });
        self.liveness.unfreeze();
        Ok(())
    }

    /// Register this facet on the worker that owns `session`.
    pub fn facet(self: &Arc<Self>, session: String) -> Result<Facet> {
        let boundary = self.clone();
        let contract = worker_boundary_contract()?;
        Ok(define_facet(move |host| {
            let boundary = boundary.clone();
            let session = session.clone();
            host.serve(
                contract.clone(),
                move |params, principal, _| {
                    if !pi_rs_chord::data::same_principal(principal, &json!({"kind":"server"}))? {
                        return Err(ChordError::new("forbidden", "server lifecycle only").into());
                    }
                    if params.to_serde()?.get("session").and_then(Value::as_str)
                        != Some(session.as_str())
                    {
                        return Err(Error::Invalid("worker lifecycle address mismatch".into()));
                    }

                    let mut members = Members::variant("server");
                    let prepare = boundary.clone();
                    members.method("prepare", move |args, ctx| {
                        let prepare = prepare.clone();
                        Box::pin(async move {
                            #[derive(Deserialize)]
                            struct PrepareArgs {
                                to: Location,
                            }
                            let args: Option<PrepareArgs> =
                                serde_json::from_value(args.to_serde()?)?;
                            let result = prepare
                                .prepare(args.map(|args| args.to), ctx.context)
                                .await?;
                            Ok(JsonValue::from_serde(serde_json::to_value(result)?))
                        })
                    })?;
                    let cancel = boundary.clone();
                    members.method("cancel", move |_, _| {
                        let cancel = cancel.clone();
                        Box::pin(async move {
                            cancel.cancel().await;
                            Ok(JsonValue::Null)
                        })
                    })?;
                    let relocate = boundary.clone();
                    members.method("relocate", move |args, ctx| {
                        let relocate = relocate.clone();
                        Box::pin(async move {
                            let location = serde_json::from_value(args.to_serde()?)?;
                            relocate.relocate(location, ctx.context).await?;
                            Ok(JsonValue::Null)
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }))
    }
}
