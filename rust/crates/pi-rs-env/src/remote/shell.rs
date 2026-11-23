use super::*;
use crate::watch as transport;
use std::{sync::PoisonError, time::Duration};
use tokio::sync::mpsc;
#[async_trait]
impl FileWatcher for transport::RemoteWatcher {
    fn mode(&self) -> WatchMode {
        match self.mode() {
            transport::WatchMode::Native => WatchMode::Native,
            transport::WatchMode::Polling => WatchMode::Polling,
        }
    }
    async fn stop(&mut self, _: &Context) {
        if let Some(thread) = self.begin_stop() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}
struct DurableWatcher(transport::RemoteWatcher);
#[async_trait]
impl FileWatcher for DurableWatcher {
    fn mode(&self) -> WatchMode {
        FileWatcher::mode(&self.0)
    }
    async fn stop(&mut self, ctx: &Context) {
        FileWatcher::stop(&mut self.0, ctx).await;
    }
}
impl Drop for DurableWatcher {
    fn drop(&mut self) {
        if let Some(thread) = self.0.begin_stop() {
            detached_cleanup(move || {
                let _ = thread.join();
            });
        }
    }
}
pub(super) async fn watch(
    env: &RemoteExecutionEnv,
    targets: &[WatchTarget],
    callback: WatchCallback,
    ctx: &Context,
) -> Result<Box<dyn FileWatcher>, FileError> {
    check(ctx, "")?;
    let mut resolved = Vec::new();
    for target in targets {
        resolved.push(transport::WatchTarget {
            path: env.absolute_path(&target.path, ctx).await?,
            recursive: target.recursive,
            exclude: target.exclude.as_ref().map(|e| transport::WatchExclude {
                hidden: e.hidden,
                names: e.names.clone(),
            }),
        });
    }
    let callback = Mutex::new(callback);
    let connection = env.connection().clone();
    let options = env.options.watch.clone();
    let mut w = transport("", move || {
        transport::RemoteWatcher::open(
            connection,
            resolved,
            Arc::new(move |e| {
                callback.lock().unwrap_or_else(PoisonError::into_inner)(match e {
                    transport::FileChange::Paths(paths) => FileChange::Paths { paths },
                    transport::FileChange::Overflow => FileChange::Overflow,
                    transport::FileChange::Failed { code, message } => FileChange::Failed {
                        error: FileError::new(code.into(), message, None),
                    },
                })
            }),
            options,
        )
        .map(DurableWatcher)
    })
    .await?;
    if ctx.is_aborted() {
        w.stop(ctx).await;
        check(ctx, "")?;
    }
    Ok(Box::new(w))
}
#[async_trait]
impl Shell for RemoteExecutionEnv {
    async fn exec(
        &self,
        command: ShellCommand<'_>,
        options: Option<ShellExecOptions<'_>>,
        ctx: &Context,
    ) -> Result<ShellExecResult, ExecutionError> {
        let mut options = options.unwrap_or_default();
        let aborted = || ExecutionError::new(ExecutionErrorCode::Aborted, "aborted");
        if ctx.is_aborted() {
            return Err(aborted());
        }
        let timeout = options.timeout;
        if let Some(n) = timeout {
            if !n.is_finite() || n <= 0. {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::Timeout,
                    "Invalid timeout: must be a finite number of seconds",
                ));
            }
            if n * 1000. > i32::MAX as f64 {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::Timeout,
                    "Invalid timeout: maximum is 2147483.647 seconds",
                ));
            }
        }
        let deadline = timeout.map(|n| tokio::time::Instant::now() + Duration::from_secs_f64(n));
        let timeout_error = || {
            ExecutionError::new(
                ExecutionErrorCode::Timeout,
                format!("timeout:{}", timeout.unwrap_or_default()),
            )
        };
        let startup = remote_info(self.connection(), self.cwd(), ctx);
        let info = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, startup)
                .await
                .map_err(|_| timeout_error())?,
            None => startup.await,
        }
        .map_err(|e| {
            if ctx.is_aborted() {
                aborted()
            } else {
                ExecutionError::new(ExecutionErrorCode::Unknown, e.message)
            }
        })?;
        let inherit = options.inherit_env.unwrap_or(true);
        let mut vars = if inherit {
            self.options.shell_env.clone()
        } else {
            BTreeMap::new()
        };
        if let Some(extra) = options.env {
            vars.extend(extra.clone());
        }
        let mut j = json!({"cwd":self.cwd(),"inheritEnv":inherit,"env":vars});
        if let Some(cwd) = options.cwd.filter(|s| !s.is_empty()) {
            j["cwd"] = json!(paths::resolve(&info, self.cwd(), cwd));
        }
        match command {
            ShellCommand::Script(s) => j["command"] = json!(s),
            ShellCommand::Argv(a) => j["argv"] = json!(a),
        }
        if let Some(shell) = &self.options.shell_path {
            j["shellPath"] = json!(shell);
        }
        if let Some(n) = timeout {
            j["timeoutMs"] = json!((n * 1000.) as u64);
        }
        if let Some(spill) = options.spill {
            j["spill"] = json!(spill);
        }
        if let Some(window) = options.window {
            j["window"] = json!(window);
        }
        let controller = Arc::new(crate::AbortController::new());
        let cancel = controller.clone();
        let _subscription = ctx
            .abort_signal()
            .map(|s| s.subscribe(move |_| cancel.abort()));
        let signal = controller.signal();
        let conn = self.connection().clone();
        let running = self.running.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let on_event = options.on_output.is_some().then(|| {
            Arc::new(move |event: &Value, data: &[u8]| {
                if event["kind"] == "output" && !data.is_empty() {
                    let _ = tx.send((event.clone(), data.to_vec()));
                }
            }) as crate::connection::EventCallback
        });
        let task = tokio::task::spawn_blocking(move || {
            let id = Arc::new(Mutex::new(None));
            let started = id.clone();
            let registry = running.clone();
            let r = conn.request(
                "exec",
                j,
                RequestOptions {
                    signal: Some(signal),
                    on_event,
                    on_start: Some(Arc::new(move |id, session| {
                        *started.lock().unwrap_or_else(PoisonError::into_inner) = Some(id);
                        registry
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(id, session);
                    })),
                    ..Default::default()
                },
            );
            if let Some(id) = *id.lock().unwrap_or_else(PoisonError::into_inner) {
                running
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
            }
            r
        });
        let run = async {
            let mut failed = None;
            while let Some((event, bytes)) = rx.recv().await {
                if failed.is_some() {
                    continue;
                }
                let info = ShellOutputInfo {
                    stream: if event["stream"] == "stderr" {
                        ShellOutputStream::Stderr
                    } else {
                        ShellOutputStream::Stdout
                    },
                    skipped: event
                        .get("skipped")
                        .and_then(|s| serde_json::from_value(s.clone()).ok()),
                };
                if let Some(cb) = options.on_output.as_mut()
                    && let Err(e) = cb(&String::from_utf8_lossy(&bytes), ctx, info)
                {
                    let mut error =
                        ExecutionError::new(ExecutionErrorCode::CallbackError, e.message.clone());
                    error.cause = Some(Box::new(e));
                    failed = Some(error);
                    controller.abort();
                }
            }
            let reply = task
                .await
                .map_err(|e| ExecutionError::new(ExecutionErrorCode::Unknown, e.to_string()))?;
            if let Some(error) = failed {
                return Err(error);
            }
            let r = reply.map_err(|e| {
                let code = match e.code.as_str() {
                    "timeout" => ExecutionErrorCode::Timeout,
                    "aborted" => ExecutionErrorCode::Aborted,
                    "spawn_error" => ExecutionErrorCode::SpawnError,
                    "shell_unavailable" => ExecutionErrorCode::ShellUnavailable,
                    _ => ExecutionErrorCode::Unknown,
                };
                let mut error = ExecutionError::new(
                    code,
                    if code == ExecutionErrorCode::Timeout {
                        format!("timeout:{}", timeout.unwrap_or_default())
                    } else {
                        e.message
                    },
                );
                error.spill_path = e.fields["spillPath"].as_str().map(str::to_owned);
                error
            })?;
            serde_json::from_value(r.json)
                .map_err(|e| ExecutionError::new(ExecutionErrorCode::Unknown, e.to_string()))
        };
        tokio::pin!(run);
        if let Some(deadline) = deadline {
            if let Ok(result) = tokio::time::timeout_at(deadline, &mut run).await {
                return result;
            }
            controller.abort();
            let mut error = timeout_error();
            if let Err(stopped) = run.await {
                if stopped.code == ExecutionErrorCode::CallbackError {
                    return Err(stopped);
                }
                error.spill_path = stopped.spill_path;
            }
            Err(error)
        } else {
            run.await
        }
    }
    async fn cleanup(&self, _: &Context) {
        let active =
            std::mem::take(&mut *self.running.lock().unwrap_or_else(PoisonError::into_inner));
        let connection = self.connection().clone();
        let _ = tokio::task::spawn_blocking(move || {
            for (id, session) in active {
                connection.kill(id, session);
            }
        })
        .await;
    }
}
