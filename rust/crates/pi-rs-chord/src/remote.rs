//! Bidirectional remote references over Chord channels, with per-member Delta dictionaries.
use crate::{
    Error, Result,
    context::{AbortSubscription, Cancel, Context, lock},
    contracts::Contract,
    data::{ChordError, FailureCause, FailureKind},
    delta::{Decoder, Encoder, Revision},
    host::{Caller, Held, Host, RefState},
    json::JsonValue,
    link::{
        Body, BoundEntry, Channel, ChannelHandler, ChannelOptions, Codec, FailedMember, Limits,
        Link, Message, Part, Payload, StateEntry,
    },
    state::{Batch, NativeSource, Source, Subscription},
    sync::SyncFuture,
    watch::{NativeWatchCall, WatchCall},
};
use indexmap::IndexMap;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::{Semaphore, oneshot, watch};
mod provider;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkRole {
    Parent,
    Child,
    Route,
}
#[derive(Clone)]
pub struct RemoteOptions {
    pub role: LinkRole,
    pub principal: serde_json::Value,
    pub trusted: bool,
    pub codec: Codec,
    pub call_window: usize,
    pub report: Arc<dyn Fn(String) + Send + Sync>,
}
impl RemoteOptions {
    pub fn new(principal: serde_json::Value) -> Self {
        Self {
            role: LinkRole::Parent,
            principal,
            trusted: false,
            codec: Codec::Binary,
            call_window: 64,
            report: Arc::new(|_| {}),
        }
    }
}
struct Outgoing {
    hold: u64,
    reply: oneshot::Sender<Result<JsonValue>>,
}
struct Serving {
    held: Held,
    cancel: Cancel,
    _observation: Option<crate::state::NativeObservation<RefState>>,
    subscriptions: Vec<Subscription>,
    methods: Vec<String>,
}
struct Incoming {
    hold: u64,
    cancel: Cancel,
}
#[derive(Default)]
struct ReplicaFlow {
    pending: Vec<StateEntry>,
    scheduled: bool,
    in_flight: usize,
    queued: std::collections::VecDeque<Body>,
}
struct Inner {
    host: Option<crate::host::WeakHost>,
    batch: Batch,
    options: RemoteOptions,
    incarnation: [u8; 16],
    channel: Mutex<Option<Arc<Channel>>>,
    epoch: AtomicU64,
    next_hold: AtomicU64,
    next_call: AtomicU64,
    holds: Mutex<BTreeMap<u64, Weak<Reference>>>,
    outgoing: Mutex<BTreeMap<u64, Outgoing>>,
    serving: Mutex<BTreeMap<u64, Serving>>,
    incoming: Mutex<BTreeMap<u64, Incoming>>,
    window: Mutex<Arc<Semaphore>>,
    closed: AtomicBool,
    replica: Mutex<ReplicaFlow>,
}
#[derive(Clone)]
pub struct RemotePeer(Arc<Inner>);
struct Handler {
    peer: Weak<Inner>,
    epoch: u64,
}
impl ChannelHandler for Handler {
    fn opened(&self) {
        if let Some(inner) = self.peer.upgrade() {
            if inner.epoch.load(Ordering::Acquire) != self.epoch {
                return;
            }
            let peer = RemotePeer(inner);
            let calls = lock(&peer.0.channel)
                .as_ref()
                .and_then(|c| c.peer())
                .and_then(|(_, calls)| calls);
            *lock(&peer.0.window) = Arc::new(Semaphore::new(
                calls.and_then(|n| usize::try_from(n).ok()).unwrap_or(65536),
            ));
            let holds = lock(&peer.0.holds)
                .values()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            for held in holds {
                if let Err(error) = peer.send_hold(&held) {
                    (peer.0.options.report)(error.to_string());
                }
            }
        }
    }
    fn message(&self, message: Message) -> SyncFuture<()> {
        let peer = self.peer.upgrade().map(RemotePeer);
        let epoch = self.epoch;
        Box::pin(async move {
            if let Some(peer) = peer
                && peer.0.epoch.load(Ordering::Acquire) == epoch
            {
                peer.message(message).await?;
            }
            Ok(())
        })
    }
    fn closed(&self) {
        if let Some(inner) = self.peer.upgrade()
            && inner.epoch.load(Ordering::Acquire) == self.epoch
        {
            RemotePeer(inner).dropped();
        }
    }
}
struct Reference {
    id: u64,
    contract: Arc<Contract>,
    params: JsonValue,
    peer: Weak<Inner>,
    state: NativeSource<RefState>,
    changed: watch::Sender<u64>,
    variant: Mutex<Option<String>>,
    members: Mutex<IndexMap<String, Source>>,
    decoders: Mutex<Vec<Decoder>>,
    released: AtomicBool,
    abort: Mutex<Option<AbortSubscription>>,
}
#[derive(Clone)]
pub struct RemoteReference(Arc<Reference>);
impl RemotePeer {
    pub fn connect(link: Arc<dyn Link>, options: RemoteOptions, batch: Batch) -> Result<Self> {
        Self::make(None, link, options, batch)
    }
    pub fn serve(host: Host, link: Arc<dyn Link>, options: RemoteOptions) -> Result<Self> {
        let batch = host.batch();
        Self::make(Some(host), link, options, batch)
    }
    fn make(
        host: Option<Host>,
        link: Arc<dyn Link>,
        options: RemoteOptions,
        batch: Batch,
    ) -> Result<Self> {
        crate::data::principal_key(&options.principal)?;
        if options.call_window == 0 {
            return Err(Error::Invalid("call window is positive".into()));
        }
        let id = uuid::Uuid::parse_str(&crate::jobs::uuid_v7()?)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let peer = Self(Arc::new(Inner {
            host: host.as_ref().map(Host::downgrade),
            batch,
            options,
            incarnation: *id.as_bytes(),
            channel: Mutex::new(None),
            epoch: AtomicU64::new(0),
            next_hold: AtomicU64::new(0),
            next_call: AtomicU64::new(0),
            holds: Mutex::new(BTreeMap::new()),
            outgoing: Mutex::new(BTreeMap::new()),
            serving: Mutex::new(BTreeMap::new()),
            incoming: Mutex::new(BTreeMap::new()),
            window: Mutex::new(Arc::new(Semaphore::new(0))),
            closed: AtomicBool::new(false),
            replica: Mutex::new(ReplicaFlow::default()),
        }));
        peer.reconnect(link)?;
        Ok(peer)
    }
    pub fn reconnect(&self, link: Arc<dyn Link>) -> Result<()> {
        if self.0.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        let epoch = self.0.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let old = lock(&self.0.channel).take();
        if let Some(old) = old {
            old.close();
            self.dropped();
        }
        let report = self.0.options.report.clone();
        let channel = Channel::start(
            link,
            Arc::new(Handler {
                peer: Arc::downgrade(&self.0),
                epoch,
            }),
            ChannelOptions {
                codec: self.0.options.codec,
                incarnation: self.0.incarnation,
                calls: if self.0.options.trusted {
                    None
                } else {
                    Some(self.0.options.call_window as u64)
                },
                report: Arc::new(move |error| report(error.to_string())),
            },
        )?;
        *lock(&self.0.channel) = Some(channel);
        Ok(())
    }
    fn channel(&self) -> Result<Arc<Channel>> {
        lock(&self.0.channel)
            .clone()
            .filter(|c| !c.closed())
            .ok_or_else(|| ChordError::new("unavailable", "link is closed").into())
    }
    fn send(&self, body: Body) -> Result<()> {
        self.channel()?.send(Message::new(body))
    }
    fn send_replica(&self, body: Body) -> Result<()> {
        {
            let mut flow = lock(&self.0.replica);
            if flow.in_flight == 2 {
                flow.queued.push_back(body);
                return Ok(());
            }
            flow.in_flight += 1;
        }
        self.send(body)
    }
    fn acknowledge_replica(&self) -> Result<()> {
        let next = {
            let mut flow = lock(&self.0.replica);
            if flow.in_flight == 0 {
                return Err(Error::Invalid("unsolicited replica acknowledgement".into()));
            }
            flow.in_flight -= 1;
            let next = flow.queued.pop_front();
            if next.is_some() {
                flow.in_flight += 1;
            }
            next
        };
        if let Some(next) = next {
            self.send(next)?;
        }
        Ok(())
    }
    fn queue_state(&self, entry: StateEntry) -> Result<()> {
        {
            let mut flow = lock(&self.0.replica);
            flow.pending.push(entry);
            if flow.scheduled {
                return Ok(());
            }
            flow.scheduled = true;
        }
        let peer = self.clone();
        let epoch = self.0.epoch.load(Ordering::Acquire);
        self.0.batch.flushing(move |_| {
            if peer.0.epoch.load(Ordering::Acquire) != epoch {
                return Ok(());
            }
            let entries = {
                let mut flow = lock(&peer.0.replica);
                flow.scheduled = false;
                std::mem::take(&mut flow.pending)
            };
            if !entries.is_empty() {
                peer.send_replica(Body::State(entries))?;
            }
            Ok(())
        });
        Ok(())
    }
    pub fn close(&self) {
        if !self.0.closed.swap(true, Ordering::AcqRel) {
            let channel = lock(&self.0.channel).clone();
            if let Some(channel) = channel {
                channel.close();
            }
            self.dropped();
            let holds = lock(&self.0.holds)
                .values()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            for hold in holds {
                let _ = hold.transition(RefState::Ended {
                    error: ChordError::new("released", "peer closed"),
                });
            }
        }
    }
    pub async fn fence(&self) -> Result<()> {
        self.channel()?.fence().await
    }
    pub fn hold(
        &self,
        contract: Arc<Contract>,
        params: JsonValue,
        context: &Context,
    ) -> Result<RemoteReference> {
        if contract.kind() != crate::contracts::ContractKind::Remote {
            return Err(ChordError::invalid("type", "local types never cross links", None).into());
        }
        crate::contracts::check_params(contract.params(), &params, "params")?;
        if self.0.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        let id = self.0.next_hold.fetch_add(1, Ordering::AcqRel);
        let (changed, _) = watch::channel(0);
        let held = RemoteReference(Arc::new(Reference {
            id,
            contract,
            params,
            peer: Arc::downgrade(&self.0),
            state: NativeSource::new(self.0.batch.clone(), RefState::Loading, |a, b| a == b),
            changed,
            variant: Mutex::new(None),
            members: Mutex::new(IndexMap::new()),
            decoders: Mutex::new(Vec::new()),
            released: AtomicBool::new(false),
            abort: Mutex::new(None),
        }));
        lock(&self.0.holds).insert(id, Arc::downgrade(&held.0));
        if let Some(signal) = context.abort_signal() {
            let weak = Arc::downgrade(&held.0);
            *lock(&held.0.abort) = Some(signal.subscribe(move |_| {
                if let Some(held) = weak.upgrade() {
                    RemoteReference(held).release();
                }
            }));
        }
        if self.channel()?.peer().is_some() {
            self.send_hold(&held.0)?;
        }
        Ok(held)
    }
    fn send_hold(&self, held: &Reference) -> Result<()> {
        if held.released.load(Ordering::Acquire) {
            return Ok(());
        }
        let (kind, params) = if held.contract.kind() == crate::contracts::ContractKind::Remote {
            let address = held.contract.host_address(&held.params)?;
            (address.kind, address.params)
        } else {
            ("server".into(), JsonValue::Object(IndexMap::new()))
        };
        self.send(Body::Hold {
            hold: held.id,
            type_name: held.contract.name().into(),
            version: held.contract.version(),
            fingerprint: fingerprint(held.contract.fingerprint())?,
            params: Part::Text(held.params.to_json()?),
            address_kind: kind,
            address_params: Box::new(params),
            principal: None,
            incarnation: None,
            chain: None,
            limit: None,
        })
    }
    fn dropped(&self) {
        *lock(&self.0.replica) = ReplicaFlow::default();
        let serving = std::mem::take(&mut *lock(&self.0.serving));
        for (_, entry) in serving {
            entry.cancel.cancel("link dropped");
            entry.held.release();
        }
        for (_, entry) in std::mem::take(&mut *lock(&self.0.incoming)) {
            entry.cancel.cancel("link dropped");
        }
        for (_, call) in std::mem::take(&mut *lock(&self.0.outgoing)) {
            let _ = call.reply.send(Err(ChordError::new(
                "unavailable",
                "link dropped; calls are not replayed",
            )
            .into()));
        }
        let holds = lock(&self.0.holds)
            .values()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        for hold in holds {
            let _ = hold.transition(RefState::Loading);
        }
    }
    async fn message(&self, message: Message) -> Result<()> {
        if message.origin.is_some() {
            return Err(Error::Invalid(
                "origins require a relay, not a direct peer".into(),
            ));
        }
        match message.body {
            body @ (Body::Hold { .. }
            | Body::Release { .. }
            | Body::Call { .. }
            | Body::Abort { .. }
            | Body::Retry { .. }) => self.provider_message(body)?,
            Body::Bound(entries) => {
                let peer = self.clone();
                self.0.batch.within(&Context::background(), move || {
                    for entry in entries {
                        if let Some(held) = peer.reference(entry.hold) {
                            peer.bound(&held, entry)?;
                        }
                    }
                    Ok(())
                })?;
                self.send(Body::Ack { events: None })?;
            }
            Body::State(entries) => {
                let peer = self.clone();
                self.0.batch.within(&Context::background(), move || {
                    for entry in entries {
                        if let Some(held) = peer.reference(entry.hold) {
                            peer.patch(&held, entry)?;
                        }
                    }
                    Ok(())
                })?;
                self.send(Body::Ack { events: None })?;
            }
            Body::Result { call, value } => {
                if let Some(call) = lock(&self.0.outgoing).remove(&call) {
                    let value = match value {
                        Some(value) => {
                            if value.segments.is_some() {
                                return Err(Error::Invalid(
                                    "JSON reference received byte segments".into(),
                                ));
                            }
                            value.part.parse(Limits::for_link(self.0.options.trusted))?
                        }
                        None => JsonValue::Null,
                    };
                    let _ = call.reply.send(Ok(value));
                }
            }
            Body::Error { call, error } => {
                if let Some(call) = lock(&self.0.outgoing).remove(&call) {
                    let _ = call.reply.send(Err(error_from_wire(&error)?));
                }
            }
            Body::Loading { hold } => {
                if let Some(held) = self.reference(hold) {
                    held.transition(RefState::Loading)?;
                }
            }
            Body::Failed {
                hold,
                cause,
                error,
                retry_in,
            } => {
                if let Some(held) = self.reference(hold) {
                    let error = chord(error_from_wire(&error)?);
                    held.transition(RefState::Failed {
                        cause: FailureCause {
                            kind: failure_kind(cause)?,
                            error,
                        },
                        retry_at: retry_in,
                    })?;
                }
            }
            Body::Gone { hold, error } => {
                if let Some(held) = self.reference(hold) {
                    let error = chord(error_from_wire(&error)?);
                    held.transition(RefState::Ended { error })?;
                    for source in lock(&held.members).values() {
                        source.detach("ended");
                    }
                }
            }
            Body::Revoke(principal) if self.0.options.role == LinkRole::Parent => {
                if let Some(host) = self
                    .0
                    .host
                    .as_ref()
                    .and_then(crate::host::WeakHost::upgrade)
                {
                    host.revoke(&principal.to_serde()?, &Context::background())
                        .await?;
                }
            }
            Body::Ack { events: None } => self.acknowledge_replica()?,
            Body::Ack { events: Some(_) } | Body::Revoke(_) => {}
            Body::Event(events) => {
                self.send(Body::Ack { events: Some(1) })?;
                if !events.is_empty() {
                    return Err(Error::Invalid(
                        "event members are not subscribed by this reference".into(),
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn reference(&self, id: u64) -> Option<Arc<Reference>> {
        lock(&self.0.holds).get(&id).and_then(Weak::upgrade)
    }
    fn bound(&self, held: &Reference, entry: BoundEntry) -> Result<()> {
        let variant = held
            .contract
            .variant_order()
            .get(entry.variant as usize)
            .cloned()
            .ok_or_else(|| Error::Invalid("bound: unknown variant index".into()))?;
        let names = held.contract.member_names(Some(&variant))?.state;
        let mut decoders = (0..names.len())
            .map(|_| Decoder::default())
            .collect::<Vec<_>>();
        let mut sources = lock(&held.members);
        let present = entry
            .members
            .iter()
            .map(|(i, _)| *i)
            .chain(entry.failed.iter().map(|f| f.member))
            .collect::<std::collections::BTreeSet<_>>();
        if present.len() != names.len() || entry.members.len() + entry.failed.len() != names.len() {
            return Err(Error::Invalid(
                "bound: missing or duplicate state members".into(),
            ));
        }
        for (index, part) in entry.members {
            let index = index as usize;
            let name = names
                .get(index)
                .ok_or_else(|| Error::Invalid("bound: unknown state index".into()))?;
            let value = part.parse(Limits::for_link(self.0.options.trusted))?;
            let source = sources
                .entry(name.clone())
                .or_insert_with(|| Source::framed(self.0.batch.clone(), name, |_| {}));
            source.address_revision(
                Revision::from_json(value.clone())?,
                vec![crate::delta::Op::Replace(value)],
                false,
                &Context::background(),
            )?;
            decoders[index] = Decoder::default();
        }
        for failed in entry.failed {
            let name = names
                .get(failed.member as usize)
                .ok_or_else(|| Error::Invalid("bound: unknown failed index".into()))?;
            sources
                .entry(name.clone())
                .or_insert_with(|| Source::framed(self.0.batch.clone(), name, |_| {}))
                .fail(
                    error_from_wire(&failed.error)?.to_string(),
                    &Context::background(),
                )?;
        }
        for (name, source) in sources.iter() {
            if !names.contains(name) {
                source.fail(
                    "member is absent from the new variant",
                    &Context::background(),
                )?;
            }
        }
        drop(sources);
        *lock(&held.decoders) = decoders;
        *lock(&held.variant) = Some(variant);
        held.transition(RefState::Live)
    }
    fn patch(&self, held: &Reference, entry: StateEntry) -> Result<()> {
        let variant = lock(&held.variant)
            .clone()
            .ok_or_else(|| Error::Invalid("state arrived before bound".into()))?;
        let names = held.contract.member_names(Some(&variant))?.state;
        for (index, part) in entry.members {
            let index = index as usize;
            let name = names
                .get(index)
                .ok_or_else(|| Error::Invalid("state: unknown member index".into()))?;
            let JsonValue::Array(wire) = part.parse(Limits::for_link(self.0.options.trusted))?
            else {
                return Err(Error::Invalid("state: changes are an array".into()));
            };
            let ops = lock(&held.decoders)
                .get_mut(index)
                .ok_or_else(|| Error::Invalid("state decoder is absent".into()))?
                .decode(&wire)?;
            let source = lock(&held.members)
                .get(name)
                .cloned()
                .ok_or_else(|| Error::Invalid("state source is absent".into()))?;
            let before = match source.value() {
                Ok(value) => value.to_json(),
                Err(_) if matches!(ops.first(), Some(crate::delta::Op::Replace(_))) => {
                    JsonValue::Null
                }
                Err(error) => return Err(error),
            };
            let value = crate::delta::apply_immutable(&before, &ops)?;
            source.address_revision(
                Revision::from_json(value)?,
                ops,
                entry.retired.contains(&(index as u64)),
                &Context::background(),
            )?;
        }
        for failed in entry.failed {
            let name = names
                .get(failed.member as usize)
                .ok_or_else(|| Error::Invalid("state: unknown failed member".into()))?;
            if let Some(source) = lock(&held.members).get(name) {
                source.fail(
                    error_from_wire(&failed.error)?.to_string(),
                    &Context::background(),
                )?;
            }
        }
        for retired in entry.retired {
            let name = names
                .get(retired as usize)
                .ok_or_else(|| Error::Invalid("state: unknown retired member".into()))?;
            if let Some(source) = lock(&held.members).get(name) {
                source.retire(&Context::background())?;
            }
        }
        Ok(())
    }
}
impl Reference {
    fn transition(&self, state: RefState) -> Result<()> {
        if self.released.load(Ordering::Acquire) {
            return Ok(());
        }
        self.state.set(state, &Context::background())?;
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
        Ok(())
    }
}
impl RemoteReference {
    pub fn state(&self) -> NativeSource<RefState> {
        self.0.state.clone()
    }
    pub fn variant(&self) -> Option<String> {
        lock(&self.0.variant).clone()
    }
    pub fn member(&self, name: &str) -> Result<Source> {
        if self.0.released.load(Ordering::Acquire) {
            return Err(ChordError::new("released", "reference released").into());
        }
        lock(&self.0.members).get(name).cloned().ok_or_else(|| {
            ChordError::new("not_ready", format!("state member {name} is not bound")).into()
        })
    }
    pub async fn wait(&self, context: &Context) -> Result<()> {
        let mut changed = self.0.changed.subscribe();
        context
            .await_with(async {
                loop {
                    changed.borrow_and_update();
                    match self.0.state.value() {
                        RefState::Live => return Ok(()),
                        RefState::Ended { error }
                        | RefState::Failed {
                            cause:
                                FailureCause {
                                    kind: FailureKind::Broken | FailureKind::Denied,
                                    error,
                                },
                            ..
                        } => return Err(error.into()),
                        _ => {}
                    }
                    changed.changed().await.map_err(|_| Error::Closed)?;
                }
            })
            .await
            .map_err(|e| ChordError::new("aborted", e.to_string()))?
    }
    pub async fn call(
        &self,
        method: &str,
        args: JsonValue,
        context: &Context,
    ) -> Result<JsonValue> {
        self.wait(context).await?;
        let peer = RemotePeer(self.0.peer.upgrade().ok_or(Error::Closed)?);
        let variant = self
            .variant()
            .ok_or_else(|| ChordError::new("not_ready", "reference is not bound"))?;
        let names = self.0.contract.member_names(Some(&variant))?;
        let method = names
            .methods
            .iter()
            .position(|n| n == method)
            .ok_or_else(|| ChordError::invalid("member", "unknown method", None))?;
        let semaphore = lock(&peer.0.window).clone();
        let _permit = context
            .await_with(semaphore.acquire_owned())
            .await
            .map_err(|e| ChordError::new("aborted", e.to_string()))?
            .map_err(|_| Error::Closed)?;
        let call = peer.0.next_call.fetch_add(1, Ordering::AcqRel);
        let (reply, receive) = oneshot::channel();
        lock(&peer.0.outgoing).insert(
            call,
            Outgoing {
                hold: self.0.id,
                reply,
            },
        );
        let mut guard = OutgoingGuard {
            peer: peer.clone(),
            call,
            armed: true,
        };
        peer.send(Body::Call {
            call,
            hold: self.0.id,
            method: method as u64,
            args: if args == JsonValue::Null {
                None
            } else {
                Some(Payload {
                    part: Part::Text(args.to_json()?),
                    segments: None,
                })
            },
        })?;
        let result = context
            .await_with(receive)
            .await
            .map_err(|e| ChordError::new("aborted", e.to_string()))?
            .map_err(|_| Error::Closed)?;
        guard.armed = false;
        result
    }
    pub fn retry(&self) -> Result<()> {
        RemotePeer(self.0.peer.upgrade().ok_or(Error::Closed)?).send(Body::Retry {
            hold: self.0.id,
            hops: 16,
        })
    }
    pub fn release(&self) {
        if !self.0.released.swap(true, Ordering::AcqRel) {
            lock(&self.0.abort).take();
            let _ = self.0.state.set(
                RefState::Ended {
                    error: ChordError::new("released", "reference released"),
                },
                &Context::background(),
            );
            self.0.changed.send_modify(|n| *n = n.wrapping_add(1));
            for source in lock(&self.0.members).values() {
                source.detach("released");
            }
            if let Some(peer) = self.0.peer.upgrade() {
                RemotePeer(peer).release(self.0.id);
            }
        }
    }
}
struct OutgoingGuard {
    peer: RemotePeer,
    call: u64,
    armed: bool,
}
impl Drop for OutgoingGuard {
    fn drop(&mut self) {
        if self.armed && lock(&self.peer.0.outgoing).remove(&self.call).is_some() {
            let _ = self.peer.send(Body::Abort { call: self.call });
        }
    }
}
impl Drop for Reference {
    fn drop(&mut self) {
        if !self.released.load(Ordering::Acquire)
            && let Some(peer) = self.peer.upgrade()
        {
            RemotePeer(peer).release(self.id);
        }
    }
}
impl RemotePeer {
    fn release(&self, id: u64) {
        lock(&self.0.holds).remove(&id);
        let _ = self.send(Body::Release { hold: id });
        let ids = lock(&self.0.outgoing)
            .iter()
            .filter_map(|(call, c)| (c.hold == id).then_some(*call))
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(call) = lock(&self.0.outgoing).remove(&id) {
                let _ = call
                    .reply
                    .send(Err(ChordError::new("released", "reference released").into()));
            }
        }
    }
}
pub fn wire_error(error: &Error) -> Result<JsonValue> {
    Ok(match error {
        Error::Chord(error) if crate::data::error_reasons(&error.code).is_some() => {
            JsonValue::from_serde(serde_json::json!({"kind":"chord","error":error}))
        }
        other => JsonValue::from_serde(
            serde_json::json!({"kind":"plugin","code":"internal","message":other.to_string()}),
        ),
    })
}
pub fn error_from_wire(error: &JsonValue) -> Result<Error> {
    let value = error.to_serde()?;
    match value.get("kind").and_then(serde_json::Value::as_str) {
        Some("chord") => Ok(ChordError::from_json(
            value
                .get("error")
                .cloned()
                .ok_or_else(|| Error::Invalid("missing chord error".into()))?,
        )?
        .into()),
        Some("plugin") => Ok(Error::Invalid(
            value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| Error::Invalid("malformed plugin error".into()))?
                .into(),
        )),
        _ => Err(Error::Invalid("unknown wire error kind".into())),
    }
}
fn chord(error: Error) -> ChordError {
    match error {
        Error::Chord(error) => *error,
        other => ChordError::new("unavailable", other.to_string()),
    }
}
fn fingerprint(value: &str) -> Result<[u8; 8]> {
    if value.len() != 16 || !value.is_ascii() {
        return Err(Error::Invalid("invalid fingerprint".into()));
    }
    let mut bytes = [0; 8];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|e| Error::Invalid(e.to_string()))?;
    }
    Ok(bytes)
}
fn failure_kind(cause: u8) -> Result<FailureKind> {
    match cause {
        0 => Ok(FailureKind::Transient),
        1 => Ok(FailureKind::Broken),
        2 => Ok(FailureKind::Waiting),
        3 => Ok(FailureKind::Denied),
        _ => Err(Error::Invalid("unknown failure cause".into())),
    }
}
