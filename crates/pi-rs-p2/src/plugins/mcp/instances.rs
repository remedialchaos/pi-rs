//! Reconcile MCP clients with live leases and reconnect failed transports.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use indexmap::{IndexMap, IndexSet};
use pi_rs_chord::{Error, Result, context::Context, data::ChordError};
use pi_rs_mcp::{
    AbortController, CallToolResult, McpClient, McpClientError, McpClientOptions,
    McpRequestOptions, McpTransportError, Roots,
};
use serde_json::{Value, json};
use tokio::{
    sync::{Notify, watch},
    task::JoinHandle,
};

use super::{
    placement::{Connect, InstanceSpec, Placement},
    transports::{self, Authorize, ConnectRemote},
};

#[derive(Clone)]
pub struct Instances {
    inner: Arc<Inner>,
}

struct Inner {
    running: Mutex<IndexMap<String, Running>>,
    remembered: Mutex<IndexMap<String, Status>>,
    changed: watch::Sender<u64>,
    remote: ConnectRemote,
    auth: Authorize,
}

struct Running {
    spec: InstanceSpec,
    state: Arc<Mutex<Status>>,
    cancel: watch::Sender<bool>,
    task: JoinHandle<()>,
}

#[derive(Default)]
struct Status {
    client: Option<McpClient>,
    phase: Option<&'static str>,
    error: Option<String>,
    tools: Vec<pi_rs_mcp::Tool>,
    instructions: Option<String>,
}

impl Instances {
    pub fn new(remote: ConnectRemote, auth: Authorize, previous: Vec<Value>) -> Result<Self> {
        let remembered = previous
            .into_iter()
            .filter_map(|row| {
                let id = row["id"].as_str()?.to_owned();
                let tools = serde_json::from_value(row["tools"].clone());
                Some(tools.map(|tools| {
                    (
                        id,
                        Status {
                            tools,
                            instructions: row["instructions"].as_str().map(str::to_owned),
                            ..Default::default()
                        },
                    )
                }))
            })
            .collect::<std::result::Result<IndexMap<_, _>, _>>()?;
        Ok(Self {
            inner: Arc::new(Inner {
                running: Mutex::default(),
                remembered: Mutex::new(remembered),
                changed: watch::channel(0).0,
                remote,
                auth,
            }),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.changed.subscribe()
    }

    pub fn reconcile(&self, placement: &Placement) {
        let mut running = self
            .inner
            .running
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut needed: IndexSet<_> = placement
            .leases
            .iter()
            .flat_map(|lease| lease.servers.values())
            .collect();
        needed.extend(placement.kept.iter().filter(|id| running.contains_key(*id)));
        running.retain(|id, entry| {
            let keep = needed.contains(id);
            if !keep {
                entry.cancel.send_replace(true);
            }
            keep
        });
        for id in needed {
            if let Some(entry) = running.get_mut(id) {
                if let Some(spec) = placement.instances.get(id) {
                    entry.spec.description.clone_from(&spec.description);
                }
            } else if let Some(spec) = placement.instances.get(id) {
                let state = self
                    .inner
                    .remembered
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .shift_remove(id)
                    .unwrap_or_default();
                running.insert(
                    id.clone(),
                    self.start(spec.clone(), Arc::new(Mutex::new(state))),
                );
            }
        }
    }

    pub fn list(&self) -> Vec<Value> {
        let running = self
            .inner
            .running
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut entries: Vec<_> = running.values().collect();
        entries.sort_by(|a, b| (&a.spec.server, &a.spec.id).cmp(&(&b.spec.server, &b.spec.id)));
        entries
            .into_iter()
            .map(|entry| {
                let spec = &entry.spec;
                let state = entry
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let mut row = json!({
                    "id":spec.id, "server":spec.server, "source":spec.source,
                    "location":spec.location, "runsOn":spec.runs_on,
                    "status":state.phase.unwrap_or("starting"), "tools":state.tools,
                });
                if matches!(spec.connect, Connect::Http { oauth: Some(_), .. }) {
                    row["auth"] = json!("oauth");
                }
                for (key, value) in [
                    ("error", &state.error),
                    ("instructions", &state.instructions),
                    ("description", &spec.description),
                ] {
                    if let Some(value) = value {
                        row[key] = json!(value);
                    }
                }
                row
            })
            .collect()
    }

    pub async fn restart(&self, id: &str) -> Result<()> {
        let mut previous = {
            let mut running = self
                .inner
                .running
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let entry = running.get_mut(id).ok_or_else(|| {
                ChordError::new("not_found", format!("No MCP server instance {id}"))
            })?;
            let state = {
                let old = entry
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                Arc::new(Mutex::new(Status {
                    tools: old.tools.clone(),
                    instructions: old.instructions.clone(),
                    ..Default::default()
                }))
            };
            entry.cancel.send_replace(true);
            std::mem::replace(entry, self.start(entry.spec.clone(), state))
        };
        let _ = (&mut previous.task).await;
        Ok(())
    }

    pub async fn reconnect(&self, server: &str) {
        let ids: Vec<_> = self
            .inner
            .running
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .filter(|entry| {
                entry.spec.server == server && matches!(entry.spec.connect, Connect::Http { .. })
            })
            .map(|entry| entry.spec.id.clone())
            .collect();
        for id in ids {
            let _ = self.restart(&id).await;
        }
    }

    pub async fn call(
        &self,
        id: &str,
        tool: &str,
        args: Value,
        context: &Context,
    ) -> Result<CallToolResult> {
        let state = self
            .inner
            .running
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(id)
            .map(|entry| entry.state.clone())
            .ok_or_else(|| Error::Invalid("This MCP server is not running".into()))?;
        let client = {
            let status = state.lock().unwrap_or_else(|error| error.into_inner());
            status.client.clone().ok_or_else(|| {
                Error::Invalid(
                    if status.phase == Some("sign-in") {
                        "This server needs a sign-in"
                    } else {
                        "This MCP server is not connected"
                    }
                    .into(),
                )
            })?
        };
        let abort = AbortController::new();
        let options = McpRequestOptions {
            signal: Some(abort.signal()),
            ..Default::default()
        };
        let result = context
            .await_with(client.call_tool(tool, Some(args), &options))
            .await;
        match result {
            Ok(result) => result.map_err(|error| Error::Invalid(error.to_string())),
            Err(error) => {
                abort.abort(None);
                Err(Error::Invalid(error.to_string()))
            }
        }
    }

    pub async fn close(&self) {
        let running = std::mem::take(
            &mut *self
                .inner
                .running
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for entry in running.values() {
            entry.cancel.send_replace(true);
        }
        for mut entry in running.into_values() {
            let _ = (&mut entry.task).await;
        }
    }

    fn start(&self, spec: InstanceSpec, state: Arc<Mutex<Status>>) -> Running {
        let (cancel, stopped) = watch::channel(false);
        let task = tokio::spawn(run(
            spec.clone(),
            state.clone(),
            self.inner.remote.clone(),
            self.inner.auth.clone(),
            self.inner.changed.clone(),
            stopped,
        ));
        Running {
            spec,
            state,
            cancel,
            task,
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
    }
}

async fn run(
    spec: InstanceSpec,
    state: Arc<Mutex<Status>>,
    remote: ConnectRemote,
    auth: Authorize,
    changed: watch::Sender<u64>,
    mut stopped: watch::Receiver<bool>,
) {
    let mut delay = Duration::from_secs(1);
    loop {
        if *stopped.borrow() {
            return;
        }
        update(&state, &changed, |status| {
            status.phase = Some("starting");
            status.error = None;
        });
        let opened = tokio::select! {
            _ = stopped.changed() => return,
            opened = transports::open(&spec.connect, &remote, &auth) => opened,
        };
        let (transport, roots) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                update(&state, &changed, |status| {
                    status.phase = Some("failed");
                    status.error = Some(error.to_string());
                });
                if matches!(spec.connect, Connect::Unplaced { .. })
                    || matches!(error, Error::Chord(error) if error.code == "daemon_too_old")
                {
                    return;
                }
                tokio::select! {
                    _ = stopped.changed() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        let client = McpClient::new(McpClientOptions {
            name: "p2".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            request_timeout_ms: spec
                .timeout
                .and_then(|timeout| u64::try_from(timeout.as_millis()).ok()),
            roots: (!roots.is_empty()).then_some(Roots::Fixed(roots)),
            ..Default::default()
        });
        let closed = Arc::new(Notify::new());
        let list_changed = Arc::new(Notify::new());
        let on_close = closed.clone();
        let on_list = list_changed.clone();
        let _close = client.on_close(Arc::new(move || on_close.notify_one()));
        let _list = client.on_notification(
            "notifications/tools/list_changed",
            Arc::new(move |_| on_list.notify_one()),
        );
        let startup = async {
            let info = client.connect(transport).await?;
            let tools = if info.capabilities.tools.is_some() {
                client.list_tools(&Default::default()).await?
            } else {
                Vec::new()
            };
            Ok::<_, McpClientError>((info.instructions, tools))
        };
        let started = tokio::select! {
            _ = stopped.changed() => { client.close().await; return; },
            started = startup => started,
        };
        match started {
            Ok((instructions, tools)) => {
                update(&state, &changed, |status| {
                    status.client = Some(client.clone());
                    status.instructions = instructions;
                    status.tools = tools;
                    status.phase = Some("connected");
                });
                let since = Instant::now();
                loop {
                    tokio::select! {
                        _ = stopped.changed() => { client.close().await; return; },
                        _ = closed.notified() => break,
                        _ = list_changed.notified() => {
                            let options = McpRequestOptions::default();
                            tokio::select! {
                                _ = stopped.changed() => { client.close().await; return; },
                                result = client.list_tools(&options) => if let Ok(tools) = result {
                                    update(&state, &changed, |status| status.tools = tools);
                                }
                            }
                        },
                    }
                }
                if since.elapsed() >= Duration::from_secs(60) {
                    delay = Duration::from_secs(1);
                }
                update(&state, &changed, |status| {
                    status.client = None;
                    status.phase = Some("failed");
                    status.error = Some("MCP connection closed".into());
                });
            }
            Err(error) => {
                let auth = matches!(
                    error,
                    McpClientError::Transport(McpTransportError::AuthRequired { .. })
                );
                update(&state, &changed, |status| {
                    status.phase = Some(if auth { "sign-in" } else { "failed" });
                    status.error = Some(if auth {
                        "This server needs a sign-in".into()
                    } else {
                        error.to_string()
                    });
                });
                if auth {
                    client.close().await;
                    return;
                }
            }
        }
        client.close().await;
        tokio::select! {
            _ = stopped.changed() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(Duration::from_secs(60));
    }
}

fn update(state: &Mutex<Status>, changed: &watch::Sender<u64>, change: impl FnOnce(&mut Status)) {
    change(&mut state.lock().unwrap_or_else(|error| error.into_inner()));
    changed.send_modify(|revision| *revision = revision.wrapping_add(1));
}
