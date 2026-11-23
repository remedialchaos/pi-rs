//! Serialized channel dispatch, handshake, heartbeat and ping fences.
use super::{Body, Link, Message, Result, invalid};
use crate::{Error, sync::SyncFuture};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, watch};
#[derive(Clone, Copy, Debug)]
pub enum Codec {
    Binary,
    Text,
}
impl Codec {
    fn encode(self, m: &Message) -> Result<Vec<u8>> {
        match self {
            Self::Binary => super::encode(m),
            Self::Text => super::encode_text(m),
        }
    }
    fn decode(self, b: &[u8]) -> Result<Message> {
        match self {
            Self::Binary => super::decode(b),
            Self::Text => super::decode_text(b),
        }
    }
}
pub trait ChannelHandler: Send + Sync {
    fn opened(&self);
    fn message(&self, message: Message) -> SyncFuture<()>;
    fn closed(&self);
}
pub struct ChannelOptions {
    pub codec: Codec,
    pub incarnation: [u8; 16],
    pub calls: Option<u64>,
    pub report: Arc<dyn Fn(Error) + Send + Sync>,
}
enum Phase {
    Greeting(Vec<Vec<u8>>),
    Open { peer: [u8; 16], calls: Option<u64> },
    Closed,
}
struct State {
    phase: Phase,
    last_received: u64,
    pinged: bool,
    pings: u64,
    pongs: u64,
    fences: VecDeque<(u64, oneshot::Sender<()>)>,
}
pub struct Channel {
    link: Arc<dyn Link>,
    handler: Arc<dyn ChannelHandler>,
    options: ChannelOptions,
    state: Mutex<State>,
    closed: watch::Sender<bool>,
    clock: Instant,
    pub sent_bytes: AtomicU64,
    pub received_bytes: AtomicU64,
}
impl Channel {
    pub fn start(
        link: Arc<dyn Link>,
        handler: Arc<dyn ChannelHandler>,
        options: ChannelOptions,
    ) -> Result<Arc<Self>> {
        let (channel, dispatch) = Self::prepare(link, handler, options)?;
        dispatch();
        Ok(channel)
    }

    /// Install transport listeners before the caller publishes the channel. Dispatch
    /// queued messages only after publication so an early hello sees the current link.
    pub(crate) fn prepare(
        link: Arc<dyn Link>,
        handler: Arc<dyn ChannelHandler>,
        options: ChannelOptions,
    ) -> Result<(Arc<Self>, impl FnOnce())> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|e| invalid(&format!("channel runtime: {e}")))?;
        let hello = options.codec.encode(&Message {
            origin: None,
            trace: None,
            old: false,
            body: Body::Hello {
                protocol: 1,
                incarnation: options.incarnation,
                calls: options.calls,
            },
        })?;
        let (closed, _) = watch::channel(false);
        let (sender, mut receiver) = mpsc::unbounded_channel::<Vec<u8>>();
        let channel = Arc::new(Self {
            link,
            handler,
            options,
            state: Mutex::new(State {
                phase: Phase::Greeting(Vec::new()),
                last_received: 0,
                pinged: false,
                pings: 0,
                pongs: 0,
                fences: VecDeque::new(),
            }),
            closed,
            clock: Instant::now(),
            sent_bytes: AtomicU64::new(hello.len() as u64),
            received_bytes: AtomicU64::new(0),
        });
        // The hello goes first, including when registering the receive listener drains old frames.
        channel.link.send(hello)?;
        let weak = Arc::downgrade(&channel);
        channel.link.on_message(Arc::new(move |bytes| {
            let Some(c) = weak.upgrade() else {
                return;
            };
            {
                let mut state = c.state.lock().unwrap_or_else(|e| e.into_inner());
                if matches!(state.phase, Phase::Closed) {
                    return;
                }
                state.last_received = c.now();
                state.pinged = false;
            }
            c.received_bytes
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            let _ = sender.send(bytes);
        }));
        let weak = Arc::downgrade(&channel);
        channel.link.on_close(Arc::new(move || {
            if let Some(c) = weak.upgrade() {
                c.ended();
            }
        }));
        let weak = Arc::downgrade(&channel);
        let mut closed = channel.closed.subscribe();
        let dispatch = move || {
            runtime.spawn(async move {
                loop {
                    if *closed.borrow() {
                        return;
                    }
                    let bytes = tokio::select! {
                        biased;
                        _ = closed.changed() => return,
                        bytes = receiver.recv() => match bytes {
                            Some(bytes) => bytes,
                            None => return,
                        },
                    };
                    let Some(c) = weak.upgrade() else {
                        return;
                    };
                    let result = match c.handle(bytes) {
                        Ok(Some(handled)) => tokio::select! {
                            biased;
                            _ = closed.changed() => return,
                            result = handled => result,
                        },
                        Ok(None) => Ok(()),
                        Err(error) => Err(error),
                    };
                    if let Err(error) = result {
                        (c.options.report)(error);
                        c.close();
                        return;
                    }
                }
            });
        };
        Ok((channel, dispatch))
    }
    fn now(&self) -> u64 {
        self.clock.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
    pub fn peer(&self) -> Option<([u8; 16], Option<u64>)> {
        match self.state.lock().unwrap_or_else(|e| e.into_inner()).phase {
            Phase::Open { peer, calls } => Some((peer, calls)),
            _ => None,
        }
    }
    pub fn closed(&self) -> bool {
        *self.closed.borrow()
    }
    pub fn send(&self, message: Message) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(state.phase, Phase::Closed) {
            return Ok(());
        }
        let bytes = self.options.codec.encode(&message)?;
        self.sent_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        if let Phase::Greeting(queue) = &mut state.phase {
            queue.push(bytes);
            return Ok(());
        }
        drop(state);
        self.link.send(bytes)
    }
    pub fn close(&self) {
        self.link.close();
        self.ended();
    }
    fn ended(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(state.phase, Phase::Closed) {
            return;
        }
        state.phase = Phase::Closed;
        state.fences.clear();
        drop(state);
        self.closed.send_replace(true);
        self.handler.closed();
    }
    fn handle(&self, bytes: Vec<u8>) -> Result<Option<SyncFuture<()>>> {
        let message = self.options.codec.decode(&bytes)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(state.phase, Phase::Closed) {
            return Ok(None);
        }
        if matches!(state.phase, Phase::Greeting(_)) {
            let Body::Hello {
                protocol,
                incarnation,
                calls,
            } = message.body
            else {
                return Err(invalid("handshake: message before hello"));
            };
            if protocol != 1 {
                return Err(invalid("handshake: incompatible protocol"));
            }
            let Phase::Greeting(queue) = std::mem::replace(
                &mut state.phase,
                Phase::Open {
                    peer: incarnation,
                    calls,
                },
            ) else {
                return Err(invalid("handshake: invalid state"));
            };
            drop(state);
            for bytes in queue {
                self.link.send(bytes)?;
            }
            self.handler.opened();
            return Ok(None);
        }
        match message.body {
            Body::Hello { .. } => Err(invalid("handshake: a second hello")),
            Body::Ping => {
                drop(state);
                self.link
                    .send(self.options.codec.encode(&Message {
                        origin: None,
                        trace: None,
                        old: false,
                        body: Body::Pong,
                    })?)
                    .map(|()| None)
            }
            Body::Pong => {
                state.pongs += 1;
                while state
                    .fences
                    .front()
                    .is_some_and(|(ping, _)| *ping <= state.pongs)
                {
                    if let Some((_, sender)) = state.fences.pop_front() {
                        let _ = sender.send(());
                    }
                }
                Ok(None)
            }
            _ => {
                drop(state);
                Ok(Some(self.handler.message(message)))
            }
        }
    }
    /// The caller drives one host-wide heartbeat timer, not a timer allocated per channel.
    pub fn tick(&self, now: u64, paused: u64, ping: u64, close: u64) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(state.phase, Phase::Closed) {
            return Ok(());
        }
        if paused > 0 {
            state.last_received = state
                .last_received
                .saturating_add(paused)
                .min(now.saturating_sub(ping));
        }
        let silent = now.saturating_sub(state.last_received);
        if silent >= close {
            drop(state);
            self.close();
        } else if silent >= ping && !state.pinged && matches!(state.phase, Phase::Open { .. }) {
            state.pinged = true;
            state.pings += 1;
            drop(state);
            self.link.send(self.options.codec.encode(&Message {
                origin: None,
                trace: None,
                old: false,
                body: Body::Ping,
            })?)?;
        }
        Ok(())
    }
    pub fn heartbeat(&self, paused: Duration, ping: Duration, close: Duration) -> Result<()> {
        self.tick(
            self.now(),
            paused.as_millis() as u64,
            ping.as_millis() as u64,
            close.as_millis() as u64,
        )
    }
    pub async fn fence(&self) -> Result<()> {
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(state.phase, Phase::Closed) {
                return Err(Error::Closed);
            }
            state.pings += 1;
            let ping = state.pings;
            state.fences.push_back((ping, sender));
        }
        self.send(Message {
            origin: None,
            trace: None,
            old: false,
            body: Body::Ping,
        })?;
        receiver.await.map_err(|_| Error::Closed)
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        self.link.close();
        self.ended();
    }
}
