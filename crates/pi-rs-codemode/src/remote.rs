//! Transport-neutral remote VM protocol. Host tools never cross this boundary.
use crate::{DEFAULT_INTERRUPT_BUDGET, INLINE_MAX_STACK_SIZE, guest::*, types::*};
use futures::{
    FutureExt,
    channel::{mpsc, oneshot},
    stream::{FuturesUnordered, StreamExt},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteRequest {
    pub data: VmStartData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interrupt_budget: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RemoteReply {
    Result(VmReply),
    Stop,
}
pub type RemoteExchange = Arc<
    dyn Fn(
            Vec<VmMessage>,
        ) -> futures::future::BoxFuture<'static, Result<Option<RemoteReply>, HostError>>
        + Send
        + Sync,
>;
pub type RemoteExecutor = Arc<
    dyn Fn(
            RemoteRequest,
            RemoteExchange,
        ) -> futures::future::BoxFuture<'static, Result<(), HostError>>
        + Send
        + Sync,
>;
pub type RemoteFactory = Arc<dyn Fn() -> Result<RemoteExecutor, HostError> + Send + Sync>;
/// Default VM memory limit of a remote sandbox. Isolates have 128 MB in total.
pub const DEFAULT_REMOTE_MEMORY_LIMIT_BYTES: usize = 32 * 1024 * 1024;
#[derive(Clone)]
pub struct ServeOptions {
    pub wasm: WasmModule,
    pub max_interrupt_budget: Option<u64>,
    /// VM memory per execution; the client may only ask for less. `None` leaves the VM unbounded.
    pub max_memory_limit_bytes: Option<usize>,
}
impl ServeOptions {
    /// Serve options with the default remote memory limit of 32 MiB.
    pub fn new(wasm: WasmModule) -> Self {
        Self {
            wasm,
            max_interrupt_budget: None,
            max_memory_limit_bytes: Some(DEFAULT_REMOTE_MEMORY_LIMIT_BYTES),
        }
    }
}
/// Runs the VM on this thread. Exchange failures stop the session and never escape.
pub async fn serve_codemode_remote(
    mut request: RemoteRequest,
    exchange: RemoteExchange,
    options: ServeOptions,
) {
    let maximum = options
        .max_interrupt_budget
        .unwrap_or(DEFAULT_INTERRUPT_BUDGET);
    let budget = request.interrupt_budget.unwrap_or(maximum).min(maximum);
    request.data.memory_limit_bytes = match (
        request.data.memory_limit_bytes,
        options.max_memory_limit_bytes,
    ) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let (sender, mut messages) = mpsc::unbounded();
    let failure_sender = sender.clone();
    let stop = CancellationToken::default();
    let terminal = stop.clone();
    let post = Arc::new(move |message: VmMessage| {
        if matches!(
            message,
            VmMessage::Done { .. } | VmMessage::Crash { .. } | VmMessage::Failure { .. }
        ) {
            terminal.cancel("VM settled");
        }
        let _ = sender.unbounded_send(message);
    });
    let mut session = match VmSession::start(
        request.data,
        options.wasm,
        post,
        stop.clone(),
        None,
        Some(budget),
        INLINE_MAX_STACK_SIZE,
    ) {
        Ok(vm) => Some(vm),
        Err(error) => {
            let failure =
                if error.0.contains("interrupted") || error.0.contains("Promise job failed") {
                    CodemodeError::new(
                        ErrorKind::Timeout,
                        format!("Execution exceeded its interrupt budget of {budget}"),
                    )
                } else {
                    CodemodeError::new(ErrorKind::Sandbox, error.0)
                };
            if !stop.is_cancelled() {
                let _ = failure_sender.unbounded_send(VmMessage::Failure { error: failure });
            }
            None
        }
    };
    drop(failure_sender);
    let mut buffered = Vec::new();
    let mut chars = 0;
    let mut requests = FuturesUnordered::new();
    loop {
        futures::select_biased! {
            message=messages.next().fuse()=>{
                let Some(message)=message else{break;};
                let mut terminal=false;let flush=match &message{
                    VmMessage::Output{item}=>{chars+=match item{OutputItem::Text{text,..}=>text.encode_utf16().count(),OutputItem::Image{data,..}=>data.encode_utf16().count()};buffered.len()+1>=1000||chars>=1<<20},
                    VmMessage::Started{..}=>false,
                    VmMessage::Call{..}|VmMessage::Idle{..}=>true,
                    _=>{terminal=true;true}
                };
                buffered.push(message);
                if flush{let batch=std::mem::take(&mut buffered);chars=0;if terminal{let _=exchange(batch).await;break;}let mut request=exchange(batch);if let Some(result)=request.as_mut().now_or_never(){requests.push(futures::future::ready(result).boxed());}else{requests.push(request);}}
            },
            reply=async{if requests.is_empty(){futures::future::pending().await}else{requests.next().await}}.fuse()=>match reply{
                Some(Ok(Some(RemoteReply::Result(reply))))=>{if let Some(vm)=&mut session&&let Err(error)=vm.deliver(reply){let failure=if vm.budget_exhausted(){CodemodeError::new(ErrorKind::Timeout,format!("Execution exceeded its interrupt budget of {budget}"))}else{CodemodeError::new(ErrorKind::Sandbox,error.0)};buffered.push(VmMessage::Failure{error:failure});let _=exchange(std::mem::take(&mut buffered)).await;break;}},
                Some(Ok(Some(RemoteReply::Stop)))|Some(Err(_))=>break,
                _=>{},
            },
        }
    }
    stop.cancel("Remote stopped");
}
pub(crate) async fn run_remote(
    factory: RemoteFactory,
    budget: Option<u64>,
    data: VmStartData,
    post: Arc<dyn Fn(VmMessage) + Send + Sync>,
    stop: CancellationToken,
    mut replies: mpsc::UnboundedReceiver<VmReply>,
) {
    let target = match factory() {
        Ok(target) => target,
        Err(error) => {
            post(VmMessage::Failure {
                error: CodemodeError::new(
                    ErrorKind::Sandbox,
                    format!("Failed to reach the remote sandbox: {error}"),
                ),
            });
            return;
        }
    };
    let waiting = Arc::new(Mutex::new(HashMap::<
        u32,
        oneshot::Sender<Option<RemoteReply>>,
    >::new()));
    let settled = Arc::new(AtomicBool::new(false));
    let map = waiting.clone();
    let sink = post.clone();
    let stopped = stop.clone();
    let terminal = settled.clone();
    let exchange: RemoteExchange = Arc::new(move |messages| {
        let map = map.clone();
        let sink = sink.clone();
        let stopped = stopped.clone();
        let terminal = terminal.clone();
        async move {
            if stopped.is_cancelled() {
                return Ok(Some(RemoteReply::Stop));
            }
            let call = messages.iter().find_map(|m| match m {
                VmMessage::Call { id, .. } => Some(*id),
                _ => None,
            });
            let receiver = if let Some(id) = call {
                let (sender, receiver) = oneshot::channel();
                map.lock()
                    .map_err(|e| HostError(e.to_string()))?
                    .insert(id, sender);
                Some(receiver)
            } else {
                None
            };
            for message in messages {
                if matches!(
                    message,
                    VmMessage::Done { .. } | VmMessage::Crash { .. } | VmMessage::Failure { .. }
                ) {
                    terminal.store(true, Ordering::SeqCst);
                }
                sink(message);
            }
            if let Some(receiver) = receiver {
                Ok(receiver.await.unwrap_or(Some(RemoteReply::Stop)))
            } else {
                Ok(None)
            }
        }
        .boxed()
    });
    let remote = target(
        RemoteRequest {
            data,
            interrupt_budget: budget,
        },
        exchange,
    )
    .fuse();
    let stopped = stop.cancelled().fuse();
    futures::pin_mut!(remote, stopped);
    loop {
        futures::select_biased! {
            result=remote=>{if !stop.is_cancelled()&&!settled.load(Ordering::SeqCst){post(VmMessage::Failure{error:CodemodeError::new(ErrorKind::Sandbox,match result{Ok(())=>"Remote sandbox returned before the script settled".into(),Err(error)=>format!("Remote sandbox failed: {error}")})});}break;},
            reply=replies.next().fuse()=>{if let Some(reply)=reply{if let Ok(mut map)=waiting.lock()&&let Some(sender)=map.remove(&reply.id){let _=sender.send(Some(RemoteReply::Result(reply)));}}else{break;}},
            _=stopped=>{if let Ok(mut map)=waiting.lock(){for(_,sender)in map.drain(){let _=sender.send(Some(RemoteReply::Stop));}}let _=remote.as_mut().now_or_never();break;},
        }
    }
}
