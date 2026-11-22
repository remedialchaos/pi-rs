//! Common host-side execution, with isolated worker and in-thread launchers.
use crate::{guest::*, identifier::to_codemode_identifier, schema::is_identifier, types::*};
use futures::{
    FutureExt,
    channel::mpsc,
    stream::{FuturesUnordered, StreamExt},
};
use indexmap::IndexMap;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
pub const JOBS_PER_POLL: usize = 50;
pub const MAX_OUTPUT_CHARS: usize = 16 * 1024 * 1024;
pub const MAX_OUTPUT_ITEMS: usize = 100_000;
pub const MAX_STORE_VALUE_CHARS: usize = 256 * 1024;
pub const MAX_STORE_TOTAL_CHARS: usize = 1024 * 1024;
pub const DEFAULT_INTERRUPT_BUDGET: u64 = 100_000;
pub const INLINE_MAX_STACK_SIZE: usize = 256 * 1024;
#[derive(Clone)]
enum Backend {
    Worker,
    Remote {
        factory: crate::remote::RemoteFactory,
        budget: Option<u64>,
    },
    Inline {
        module: WasmModule,
        budget: u64,
    },
}
#[derive(Clone)]
pub struct CodemodeSandbox {
    options: Arc<Mutex<SandboxOptions>>,
    closed: CancellationToken,
    backend: Backend,
}
impl Default for CodemodeSandbox {
    fn default() -> Self {
        Self {
            options: Arc::new(Mutex::new(SandboxOptions::default())),
            closed: CancellationToken::default(),
            backend: Backend::Worker,
        }
    }
}
impl CodemodeSandbox {
    pub fn remote(
        options: SandboxOptions,
        factory: crate::remote::RemoteFactory,
        budget: Option<u64>,
    ) -> Result<Self, HostError> {
        let mut sandbox = Self::new(options)?;
        sandbox.backend = Backend::Remote { factory, budget };
        Ok(sandbox)
    }
    pub fn new(options: SandboxOptions) -> Result<Self, HostError> {
        validate_tools(&options)?;
        Ok(Self {
            options: Arc::new(Mutex::new(options)),
            closed: CancellationToken::default(),
            backend: Backend::Worker,
        })
    }
    pub fn inline(
        options: SandboxOptions,
        module: WasmModule,
        budget: Option<u64>,
    ) -> Result<Self, HostError> {
        let mut sandbox = Self::new(options)?;
        sandbox.backend = Backend::Inline {
            module,
            budget: budget.unwrap_or(DEFAULT_INTERRUPT_BUDGET),
        };
        Ok(sandbox)
    }
    pub fn close(&self) {
        self.closed.cancel("Sandbox closed");
    }
    pub fn register_tool(&self, tool: CodemodeTool) -> Result<(), HostError> {
        let mut options = self.options.lock().map_err(|e| HostError(e.to_string()))?;
        if options.tools.iter().any(|t| t.name() == tool.name()) {
            return Err(HostError(format!(
                "Tool {:?} is already registered",
                tool.name()
            )));
        }
        options.tools.push(tool);
        Ok(())
    }
    pub fn unregister_tool(&self, name: &str) -> Result<bool, HostError> {
        let mut options = self.options.lock().map_err(|e| HostError(e.to_string()))?;
        let before = options.tools.len();
        options.tools.retain(|t| t.name() != name);
        Ok(options.tools.len() != before)
    }
    pub fn tools(&self) -> Result<Vec<CodemodeTool>, HostError> {
        Ok(self
            .options
            .lock()
            .map_err(|e| HostError(e.to_string()))?
            .tools
            .clone())
    }
    pub async fn execute(
        &self,
        code: &str,
        options: ExecuteOptions,
    ) -> Result<CodemodeResult, HostError> {
        self.drive(code, options, None).await
    }
    pub fn start(&self, code: &str, options: RunOptions) -> Result<CodemodeRun, HostError> {
        if !options.started_at.is_finite() || options.started_at < 0.0 {
            return Err(HostError(
                "startedAt must be a finite non-negative number".into(),
            ));
        }
        let mut names = HashSet::new();
        let mut aliases = HashSet::new();
        for tool in &options.tools {
            if !names.insert(&tool.name) {
                return Err(HostError(format!(
                    "Tool {:?} is already declared",
                    tool.name
                )));
            }
            let alias = match &tool.js_name {
                Some(alias) => alias.clone(),
                None => to_codemode_identifier(tool.name.clone()).map_err(HostError)?,
            };
            if !aliases.insert(alias.clone()) {
                return Err(HostError(format!(
                    "Tool script names must be unique: {alias:?} is already declared"
                )));
            }
        }
        validate_globals(&options.globals)?;
        let (sender, receiver) = mpsc::unbounded();
        let control = Arc::new(Control {
            finished: CancellationToken::default(),
            options,
            delivered: Mutex::new(HashSet::new()),
            pending: Mutex::new(HashMap::new()),
            sender,
            abort: CancellationToken::default(),
        });
        let sandbox = self.clone();
        let script = code.to_owned();
        let state = control.clone();
        let promise = async move {
            let execute = ExecuteOptions {
                timeout: state.options.timeout,
                signal: state.options.signal.clone(),
                store: state.options.store.clone(),
            };
            sandbox
                .drive(&script, execute, Some((state, receiver)))
                .await
        }
        .boxed();
        Ok(CodemodeRun { promise, control })
    }
    async fn drive(
        &self,
        code: &str,
        options: ExecuteOptions,
        control: Option<(Arc<Control>, mpsc::UnboundedReceiver<VmReply>)>,
    ) -> Result<CodemodeResult, HostError> {
        if self.closed.is_cancelled() {
            return Err(HostError("Sandbox is closed".into()));
        }
        let settings = self
            .options
            .lock()
            .map_err(|e| HostError(e.to_string()))?
            .clone();
        let timeout = options.timeout.unwrap_or(settings.timeout);
        let deadline = timeout.and_then(|d| Instant::now().checked_add(d));
        let (sender, mut events) = mpsc::unbounded();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            sender,
            stop: CancellationToken::default(),
            control: control.as_ref().map(|c| c.0.clone()),
        });
        let mut cleanup = ExecutionCleanup {
            shared: shared.clone(),
            signals: Vec::new(),
        };
        let signal = options.signal.clone();
        if let Some(reason) = signal.reason() {
            shared.finish(Err(CodemodeError::new(ErrorKind::Aborted, reason)));
            return shared.result();
        }
        let tools = if let Some(c) = &shared.control {
            c.options
                .tools
                .iter()
                .map(|t| {
                    Ok(VmTool {
                        name: t.name.clone(),
                        js_name: match &t.js_name {
                            Some(n) => n.clone(),
                            None => to_codemode_identifier(t.name.clone()).map_err(HostError)?,
                        },
                        description: t.description.clone(),
                    })
                })
                .collect::<Result<Vec<_>, HostError>>()?
        } else {
            settings
                .tools
                .iter()
                .map(|t| {
                    Ok(VmTool {
                        name: t.name().into(),
                        js_name: to_codemode_identifier(t.name().into()).map_err(HostError)?,
                        description: t.declaration.description.clone(),
                    })
                })
                .collect::<Result<Vec<_>, HostError>>()?
        };
        let data = VmStartData {
            code: code.into(),
            tools,
            globals: if let Some(c) = &shared.control {
                c.options.globals.clone()
            } else {
                settings
                    .globals
                    .iter()
                    .map(|t| VmGlobal {
                        name: t.name().into(),
                        spread: t.spread,
                    })
                    .collect()
            },
            memory_limit_bytes: settings.memory_limit_bytes,
            store: options
                .store
                .into_iter()
                .map(|(k, v)| (k, v.to_string()))
                .collect(),
            deterministic: shared.control.as_ref().map(|c| Deterministic {
                started_at: c.options.started_at,
                seed: c.options.seed,
            }),
        };
        let (replies, mut receiver) = mpsc::unbounded();
        let sink = shared.clone();
        let post: Arc<dyn Fn(VmMessage) + Send + Sync> =
            Arc::new(move |message| sink.receive(message));
        let mut inline = None;
        let mut remote_task: futures::future::Fuse<futures::future::BoxFuture<'static, ()>> =
            futures::future::pending().boxed().fuse();
        match &self.backend {
            Backend::Remote { factory, budget } => {
                remote_task = crate::remote::run_remote(
                    factory.clone(),
                    *budget,
                    data,
                    post,
                    shared.stop.clone(),
                    receiver,
                )
                .boxed()
                .fuse();
            }
            Backend::Worker => {
                let path = settings.wasm_path.clone();
                let (loaded, loading) = futures::channel::oneshot::channel();
                if let Err(error) = std::thread::Builder::new()
                    .name("pi-codemode-module".into())
                    .spawn(move || {
                        let _ = loaded.send(load_quickjs_wasm(path.as_deref()));
                    })
                {
                    shared.finish(Err(CodemodeError::new(
                        ErrorKind::Sandbox,
                        format!("Failed to load QuickJS: {error}"),
                    )));
                    return shared.result();
                }
                let loading = loading.fuse();
                let expired = async {
                    match deadline {
                        Some(deadline) => {
                            futures_timer::Delay::new(
                                deadline.saturating_duration_since(Instant::now()),
                            )
                            .await
                        }
                        None => futures::future::pending().await,
                    }
                }
                .fuse();
                let aborted = signal.cancelled().fuse();
                let closed = self.closed.cancelled().fuse();
                let control_abort = shared
                    .control
                    .as_ref()
                    .map_or_else(CancellationToken::default, |c| c.abort.clone());
                let control_aborted = control_abort.cancelled().fuse();
                futures::pin_mut!(loading, expired, aborted, closed, control_aborted);
                let module = futures::select_biased! {
                    reason=aborted=>{shared.finish(Err(CodemodeError::new(ErrorKind::Aborted,reason)));return shared.result();},
                    reason=closed=>{shared.finish(Err(CodemodeError::new(ErrorKind::Aborted,reason)));return shared.result();},
                    reason=control_aborted=>{shared.finish(Err(CodemodeError::new(ErrorKind::Aborted,reason)));return shared.result();},
                    _=expired=>{shared.finish(Err(timeout_error(timeout)));return shared.result();},
                    result=loading=>match result {
                        Ok(Ok(module))=>module,
                        error=>{let message=match error{Ok(Err(e))=>e.0,Err(e)=>e.to_string(),Ok(Ok(_))=>String::new()};shared.finish(Err(CodemodeError::new(ErrorKind::Sandbox,format!("Failed to load QuickJS: {message}"))));return shared.result();}
                    }
                };
                let release = match &settings.acquire_worker {
                    Some(acquire) => match acquire() {
                        Ok(r) => Some(r),
                        Err(e) => {
                            shared.finish(Err(CodemodeError::new(ErrorKind::Sandbox, e.0)));
                            return shared.result();
                        }
                    },
                    None => None,
                };
                let exited = WorkerExit::default();
                let lifetime = WorkerLifetime {
                    release,
                    exited: exited.clone(),
                };
                let (admit, admitted) = std::sync::mpsc::channel();
                let worker_shared = shared.clone();
                let budget = if settings.loop_iteration_limit != 64 * 1024 * 1024 {
                    Some(settings.loop_iteration_limit)
                } else {
                    None
                };
                let worker = std::thread::Builder::new()
                    .name("pi-codemode".into())
                    .stack_size(4 * 1024 * 1024)
                    .spawn(move || {
                        let _lifetime = lifetime;
                        if admitted.recv() != Ok(true) {
                            return;
                        }
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                            || -> Result<(), HostError> {
                                let mut vm = VmSession::start(
                                    data,
                                    module,
                                    post,
                                    worker_shared.stop.clone(),
                                    deadline,
                                    budget,
                                    512 * 1024,
                                )?;
                                while !worker_shared.done() {
                                    match futures::executor::block_on(receiver.next()) {
                                        Some(reply) => vm.deliver(reply)?,
                                        None => break,
                                    }
                                }
                                Ok(())
                            },
                        ));
                        match result {
                            Ok(Err(e)) => worker_shared.fail_vm(e, budget, deadline),
                            Err(_) => worker_shared.finish(Err(CodemodeError::new(
                                ErrorKind::Sandbox,
                                "VM worker panicked",
                            ))),
                            Ok(Ok(())) => {}
                        }
                    });
                if let Err(e) = worker {
                    shared.finish(Err(CodemodeError::new(
                        ErrorKind::Sandbox,
                        format!("Failed to start worker: {e}"),
                    )));
                    return shared.result();
                }
                if let Some(started) = &settings.on_worker_started
                    && let Err(e) = started(exited)
                {
                    shared.finish(Err(CodemodeError::new(ErrorKind::Sandbox, e.0)));
                    let _ = admit.send(false);
                    return shared.result();
                }
                let _ = admit.send(true);
            }
            Backend::Inline { module, budget } => {
                match VmSession::start(
                    data,
                    module.clone(),
                    post,
                    shared.stop.clone(),
                    deadline,
                    Some(*budget),
                    INLINE_MAX_STACK_SIZE,
                ) {
                    Ok(vm) => inline = Some(vm),
                    Err(e) => shared.fail_vm(e, Some(*budget), deadline),
                }
            }
        }
        let mut timer = if let Some(deadline) = deadline {
            futures_timer::Delay::new(deadline.saturating_duration_since(Instant::now())).boxed()
        } else {
            futures::future::pending().boxed()
        }
        .fuse();
        let aborted = signal.cancelled().fuse();
        let closed = self.closed.cancelled().fuse();
        let control_abort = shared
            .control
            .as_ref()
            .map_or_else(CancellationToken::default, |c| c.abort.clone());
        let control_aborted = control_abort.cancelled().fuse();
        futures::pin_mut!(aborted, closed, control_aborted);
        let mut tasks = FuturesUnordered::new();
        let mut controlled = control.map(|c| c.1).unwrap_or_else(|| {
            let (_tx, rx) = mpsc::unbounded();
            rx
        });
        let mut control_open = shared.control.is_some();
        loop {
            futures::select_biased! {
                _=remote_task=>{},
                event=events.next().fuse()=>match event{
                    Some(Event::Started(fingerprint)) => { if let Some(c)=&shared.control && let Some(callback)=&c.options.on_started {callback(fingerprint);} },
                    Some(Event::Idle(during)) => { if let Some(c)=&shared.control && let Some(callback)=&c.options.on_idle {callback(during);} },
                    Some(Event::Done)=>break,
                    Some(Event::Call{id,name,global,args,record})=>{
                        if let Some(control)=&shared.control{if let Some(callback)=&control.options.on_call{callback(RunCall{id,target:if global{"global"}else{"tool"}.into(),name,args});}continue;}
                        let tool=if global{settings.globals.iter().find(|t|t.name()==name)}else{settings.tools.iter().find(|t|t.name()==name)};
                        if let Some(tool)=tool{let cancellation=CancellationToken::default();cleanup.signals.push(cancellation.clone());let future=(tool.execute)(args,ToolContext{signal:cancellation});let task_shared=shared.clone();tasks.push(async move{let outcome=future.await;if let Some(record)=record{task_shared.complete_call(record,outcome.is_ok());}VmReply{id,ok:outcome.is_ok(),payload:match outcome{Ok(value)=>value.map(|v|v.to_string()),Err(e)=>Some(e.0)},seq:None,now:None,properties:None}});
                            if let Some(Some(reply))=tasks.next().now_or_never(){deliver(&mut inline,&replies,reply,&shared,deadline);}
                        }else{deliver(&mut inline,&replies,VmReply{id,ok:false,payload:Some(format!("Unknown {} {name:?}",if global{"global"}else{"tool"})),seq:None,now:None,properties:None},&shared,deadline);}
                    }
                    None=>{shared.finish(Err(CodemodeError::new(ErrorKind::Sandbox,"VM exited before the script settled")));break;}
                },
                reply=async{if tasks.is_empty(){futures::future::pending().await}else{tasks.next().await}}.fuse()=>{if let Some(reply)=reply{deliver(&mut inline,&replies,reply,&shared,deadline);}},
                reply=async{if control_open{controlled.next().await}else{futures::future::pending().await}}.fuse()=>{if let Some(reply)=reply{shared.complete_pending(reply.id,reply.ok);deliver(&mut inline,&replies,reply,&shared,deadline);}else{control_open=false;}},
                _=timer=>{shared.finish(Err(timeout_error(timeout)));break;},
                reason=aborted=>{shared.finish(Err(CodemodeError::new(ErrorKind::Aborted,reason)));break;},
                reason=closed=>{shared.finish(Err(CodemodeError::new(ErrorKind::Aborted,reason)));break;},
                reason=control_aborted=>{shared.finish(Err(CodemodeError::new(ErrorKind::Aborted,reason)));break;},
            }
        }
        drop(tasks);
        let _ = remote_task.now_or_never();
        shared.result()
    }
}
fn deliver(
    inline: &mut Option<VmSession>,
    sender: &mpsc::UnboundedSender<VmReply>,
    reply: VmReply,
    shared: &Shared,
    deadline: Option<Instant>,
) {
    if let Some(vm) = inline {
        if let Err(e) = vm.deliver(reply) {
            shared.fail_vm(e, vm.interrupt_budget(), deadline);
        }
    } else {
        let _ = sender.unbounded_send(reply);
    }
}
fn timeout_error(timeout: Option<Duration>) -> CodemodeError {
    CodemodeError::new(
        ErrorKind::Timeout,
        timeout.map_or("Execution timed out".into(), |d| {
            format!("Execution timed out after {} ms", d.as_secs_f64() * 1000.0)
        }),
    )
}
struct WorkerLifetime {
    release: Option<WorkerRelease>,
    exited: WorkerExit,
}
impl Drop for WorkerLifetime {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
        self.exited.0.cancel("worker exited");
    }
}
fn validate_tools(options: &SandboxOptions) -> Result<(), HostError> {
    let mut names = HashSet::new();
    for tool in &options.tools {
        if !names.insert(tool.name()) {
            return Err(HostError(format!(
                "Tool {:?} is already registered",
                tool.name()
            )));
        }
    }
    validate_globals(
        &options
            .globals
            .iter()
            .map(|t| VmGlobal {
                name: t.name().into(),
                spread: t.spread,
            })
            .collect::<Vec<_>>(),
    )
}
fn validate_globals(globals: &[VmGlobal]) -> Result<(), HostError> {
    let mut names = HashSet::new();
    for global in globals {
        let name = &global.name;
        let parts: Vec<_> = name.split('.').collect();
        if parts.len() > 2
            || parts.iter().any(|p| !is_identifier(p))
            || [
                "tools",
                "ALL_TOOLS",
                "console",
                "text",
                "image",
                "exit",
                "globalThis",
                "store",
                "load",
            ]
            .contains(&parts[0])
        {
            return Err(HostError(format!("Invalid global name {name:?}")));
        }
        if !names.insert(name) {
            return Err(HostError(format!("Global {name:?} is already registered")));
        }
    }
    for name in &names {
        if let Some((namespace, _)) = name.split_once('.')
            && names.iter().any(|n| n.as_str() == namespace)
        {
            return Err(HostError(format!(
                "Global {namespace:?} conflicts with the namespace {namespace:?}"
            )));
        }
    }
    Ok(())
}
enum Event {
    Started(String),
    Idle(During),
    Call {
        id: u32,
        name: String,
        global: bool,
        args: Option<Value>,
        record: Option<usize>,
    },
    Done,
}
#[derive(Default)]
struct State {
    terminal: Option<Result<Option<Value>, CodemodeError>>,
    output: Vec<OutputItem>,
    calls: Vec<CodemodeCall>,
    started: Vec<Instant>,
    pending: HashMap<u32, usize>,
    writes: StoreWrites,
    during: Option<During>,
}
struct Shared {
    state: Mutex<State>,
    sender: mpsc::UnboundedSender<Event>,
    stop: CancellationToken,
    control: Option<Arc<Control>>,
}
struct ExecutionCleanup {
    shared: Arc<Shared>,
    signals: Vec<CancellationToken>,
}
impl Drop for ExecutionCleanup {
    fn drop(&mut self) {
        self.shared.finish(Err(CodemodeError::new(
            ErrorKind::Aborted,
            "Execution finished",
        )));
        for signal in &self.signals {
            signal.cancel("Execution finished");
        }
    }
}
impl Shared {
    fn done(&self) -> bool {
        self.state.lock().map_or(true, |s| s.terminal.is_some())
    }
    fn finish(&self, result: Result<Option<Value>, CodemodeError>) {
        if let Ok(mut state) = self.state.lock() {
            if state.terminal.is_some() {
                return;
            }
            state.terminal = Some(result);
        }
        if let Some(control) = &self.control {
            control.finished.cancel("Execution finished");
        }
        self.stop.cancel("Execution finished");
        let _ = self.sender.unbounded_send(Event::Done);
    }
    fn fail_vm(&self, error: HostError, budget: Option<u64>, deadline: Option<Instant>) {
        if self.done() {
            return;
        }
        let timeout = deadline.is_some_and(|d| Instant::now() >= d);
        let budget_hit = error.0.contains("interrupted") || error.0.contains("Promise job failed");
        self.finish(Err(if timeout {
            timeout_error(None)
        } else if let Some(budget) = budget.filter(|_| budget_hit) {
            CodemodeError::new(
                ErrorKind::Timeout,
                format!("Execution exceeded its interrupt budget of {budget}"),
            )
        } else {
            CodemodeError::new(ErrorKind::Sandbox, error.0)
        }));
    }
    fn receive(&self, message: VmMessage) {
        if self.done() {
            return;
        }
        let result = (|| -> Result<(), HostError> {
            match message {
                VmMessage::Failure { error } => self.finish(Err(error)),
                VmMessage::Output { item } => {
                    self.state
                        .lock()
                        .map_err(|e| HostError(e.to_string()))?
                        .output
                        .push(item);
                }
                VmMessage::Call {
                    id,
                    target,
                    name,
                    args,
                } => {
                    let args = args.map(|s| parse_json(&s, "call arguments")).transpose()?;
                    let global = target == "global";
                    let mut state = self.state.lock().map_err(|e| HostError(e.to_string()))?;
                    let record = if global {
                        None
                    } else {
                        let i = state.calls.len();
                        state.calls.push(CodemodeCall {
                            name: name.clone(),
                            status: CallStatus::Cancelled,
                            duration_ms: 0.0,
                        });
                        state.started.push(Instant::now());
                        state.pending.insert(id, i);
                        Some(i)
                    };
                    drop(state);
                    if let Some(control) = &self.control {
                        control
                            .pending
                            .lock()
                            .map_err(|e| HostError(e.to_string()))?
                            .insert(id, ());
                    }
                    let _ = self.sender.unbounded_send(Event::Call {
                        id,
                        name,
                        global,
                        args,
                        record,
                    });
                }
                VmMessage::Done {
                    ok,
                    value,
                    writes,
                    error,
                    during,
                } => {
                    let result = if ok {
                        let writes = parse_writes(
                            writes
                                .as_deref()
                                .ok_or_else(|| HostError("store writes missing".into()))?,
                        )?;
                        let value = value.map(|s| parse_json(&s, "return value")).transpose()?;
                        let mut state = self.state.lock().map_err(|e| HostError(e.to_string()))?;
                        state.writes = writes;
                        Ok(value)
                    } else {
                        Err(parse_error(
                            error
                                .as_deref()
                                .ok_or_else(|| HostError("script error missing".into()))?,
                        )?)
                    };
                    self.state
                        .lock()
                        .map_err(|e| HostError(e.to_string()))?
                        .during = during;
                    self.finish(result);
                }
                VmMessage::Crash { message } => {
                    self.finish(Err(CodemodeError::new(ErrorKind::Sandbox, message)))
                }
                VmMessage::Started { fingerprint } => {
                    let _ = self.sender.unbounded_send(Event::Started(fingerprint));
                }
                VmMessage::Idle { seq, issued } => {
                    let _ = self
                        .sender
                        .unbounded_send(Event::Idle(During { seq, issued }));
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.finish(Err(CodemodeError::new(ErrorKind::Sandbox, format!("Sandbox bridge broken: {}. The script may have modified built-ins such as a prototype's toJSON.", e.0))));
        }
    }
    fn complete_pending(&self, id: u32, ok: bool) {
        let index = self
            .state
            .lock()
            .ok()
            .and_then(|mut s| s.pending.remove(&id));
        if let Some(index) = index {
            self.complete_call(index, ok);
        }
    }
    fn complete_call(&self, index: usize, ok: bool) {
        if let Ok(mut s) = self.state.lock() {
            s.calls[index].duration_ms = s.started[index].elapsed().as_secs_f64() * 1000.0;
            s.calls[index].status = if ok {
                CallStatus::Ok
            } else {
                CallStatus::Error
            };
        }
    }
    fn result(&self) -> Result<CodemodeResult, HostError> {
        let mut s = self.state.lock().map_err(|e| HostError(e.to_string()))?;
        for i in 0..s.calls.len() {
            if s.calls[i].status == CallStatus::Cancelled {
                s.calls[i].duration_ms = s.started[i].elapsed().as_secs_f64() * 1000.0;
            }
        }
        match s.terminal.clone() {
            Some(Ok(value)) => Ok(CodemodeResult::Success {
                ok: true,
                value,
                output: s.output.clone(),
                calls: s.calls.clone(),
                store_writes: s.writes.clone(),
                during: s.during,
            }),
            Some(Err(error)) => Ok(CodemodeResult::Failure {
                ok: false,
                error,
                output: s.output.clone(),
                calls: s.calls.clone(),
                during: s.during,
            }),
            None => Err(HostError("Execution has not finished".into())),
        }
    }
}
fn parse_json(text: &str, what: &str) -> Result<Value, HostError> {
    serde_json::from_str(text).map_err(|_| HostError(format!("{what} is not valid JSON")))
}
fn parse_error(text: &str) -> Result<CodemodeError, HostError> {
    let value = parse_json(text, "script error")?;
    let object = value
        .as_object()
        .ok_or_else(|| HostError("script error is not an object".into()))?;
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError("script error is malformed".into()))?
        .into();
    let optional = |key| -> Result<Option<String>, HostError> {
        object
            .get(key)
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| HostError("script error is malformed".into()))
            })
            .transpose()
    };
    Ok(CodemodeError {
        kind: ErrorKind::Script,
        message,
        name: optional("name")?,
        stack: optional("stack")?,
    })
}
fn parse_writes(text: &str) -> Result<StoreWrites, HostError> {
    let value = parse_json(text, "store writes")?;
    let entries = value
        .as_array()
        .ok_or_else(|| HostError("store writes are not an array".into()))?;
    let mut writes = StoreWrites::default();
    for entry in entries {
        let entry = entry
            .as_array()
            .ok_or_else(|| HostError("store writes contain a malformed entry".into()))?;
        let key = entry
            .first()
            .and_then(Value::as_str)
            .ok_or_else(|| HostError("store writes contain a malformed entry".into()))?;
        match entry.len() {
            1 => writes.delete.push(key.into()),
            2 => {
                let text = entry[1]
                    .as_str()
                    .ok_or_else(|| HostError("store writes contain a malformed entry".into()))?;
                writes.set.insert(
                    key.into(),
                    parse_json(text, &format!("store value for {key:?}"))?,
                );
            }
            _ => return Err(HostError("store writes contain a malformed entry".into())),
        }
    }
    Ok(writes)
}
#[derive(Clone, Default)]
pub struct DeclaredTool {
    pub name: String,
    pub js_name: Option<String>,
    pub description: String,
}
#[derive(Clone)]
pub struct RunCall {
    pub id: u32,
    pub target: String,
    pub name: String,
    pub args: Option<Value>,
}
#[derive(Clone, Default)]
pub struct RunOptions {
    pub tools: Vec<DeclaredTool>,
    pub globals: Vec<VmGlobal>,
    pub started_at: f64,
    pub seed: u32,
    pub store: IndexMap<String, Value>,
    pub timeout: Option<Option<Duration>>,
    pub signal: CancellationToken,
    pub on_started: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub on_call: Option<Arc<dyn Fn(RunCall) + Send + Sync>>,
    pub on_idle: Option<Arc<dyn Fn(During) + Send + Sync>>,
}
pub enum Outcome {
    Ok(Option<Value>),
    Error {
        message: String,
        properties: Option<Value>,
    },
}
struct Control {
    finished: CancellationToken,
    options: RunOptions,
    delivered: Mutex<HashSet<u64>>,
    pending: Mutex<HashMap<u32, ()>>,
    sender: mpsc::UnboundedSender<VmReply>,
    abort: CancellationToken,
}
pub struct CodemodeRun {
    pub promise: futures::future::BoxFuture<'static, Result<CodemodeResult, HostError>>,
    control: Arc<Control>,
}
impl CodemodeRun {
    pub fn controller(&self) -> RunController {
        RunController {
            control: self.control.clone(),
        }
    }
    pub fn finished(&self) -> bool {
        self.control.finished.is_cancelled()
    }
    pub fn deliver(&self, seq: u64, now: f64, id: u32, outcome: Outcome) -> Result<(), HostError> {
        self.controller().deliver(seq, now, id, outcome)
    }
    pub fn abort(&self, message: impl Into<String>) {
        self.control.abort.cancel(message);
    }
}
#[derive(Clone)]
pub struct RunController {
    control: Arc<Control>,
}
impl RunController {
    pub fn finished(&self) -> bool {
        self.control.finished.is_cancelled()
    }
    pub fn deliver(&self, seq: u64, now: f64, id: u32, outcome: Outcome) -> Result<(), HostError> {
        if self.finished() {
            return Ok(());
        }
        if !now.is_finite() {
            return Err(HostError(format!(
                "The clock value of result {seq} is not a finite number"
            )));
        }
        let mut delivered = self
            .control
            .delivered
            .lock()
            .map_err(|e| HostError(e.to_string()))?;
        if seq == 0 || delivered.contains(&seq) {
            return Err(HostError(format!(
                "Result number {seq} is not a positive integer used once"
            )));
        }
        let mut pending = self
            .control
            .pending
            .lock()
            .map_err(|e| HostError(e.to_string()))?;
        if !pending.contains_key(&id) {
            return Err(HostError(format!("Call {id} is not waiting for a result")));
        }
        let (ok, payload, properties) = match outcome {
            Outcome::Ok(value) => (true, value.map(|v| v.to_string()), None),
            Outcome::Error {
                message,
                properties,
            } => {
                if properties.as_ref().is_some_and(|p| !p.is_object()) {
                    return Err(HostError(format!(
                        "The properties of result {seq} are not an object"
                    )));
                }
                (false, Some(message), properties.map(|p| p.to_string()))
            }
        };
        delivered.insert(seq);
        pending.remove(&id);
        self.control
            .sender
            .unbounded_send(VmReply {
                id,
                ok,
                payload,
                properties,
                seq: Some(seq),
                now: Some(now),
            })
            .map_err(|e| HostError(e.to_string()))
    }
    pub fn abort(&self, message: impl Into<String>) {
        self.control.abort.cancel(message);
    }
}
