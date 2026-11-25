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
/// A validated inbound hold and the identity established by its link.
pub struct RouteHold {
    pub address_kind: String,
    pub address_params: JsonValue,
    pub contract: Arc<Contract>,
    pub params: JsonValue,
    pub caller: Caller,
    pub context: Context,
}

pub type HoldRouter = Arc<dyn Fn(RouteHold) -> SyncFuture<Option<RemoteReference>> + Send + Sync>;

#[derive(Clone)]
pub struct RemoteOptions {
    pub role: LinkRole,
    pub principal: serde_json::Value,
    pub trusted: bool,
    pub codec: Codec,
    pub call_window: usize,
    pub report: Arc<dyn Fn(String) + Send + Sync>,
    /// Resolve a forward destination. `Ok(None)` dispatches locally.
    pub route: Option<HoldRouter>,
    /// Declarations of routable types that need not have a local implementation.
    pub route_contracts: BTreeMap<String, Arc<Contract>>,
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
            route: None,
            route_contracts: BTreeMap::new(),
        }
    }
}
struct Outgoing {
    hold: u64,
    reply: oneshot::Sender<Result<JsonValue>>,
}
struct Serving {
    key: Arc<()>,
    held: Provided,
    cancel: Cancel,
    _observation: Option<crate::state::NativeObservation<RefState>>,
    subscriptions: Vec<Subscription>,
    methods: Vec<String>,
}
#[derive(Clone)]
enum Provided {
    Local(Held),
    Routed(RemoteReference),
}
struct Incoming {
    key: Arc<()>,
    hold: u64,
    cancel: Cancel,
}
struct PendingRoute {
    key: Arc<()>,
    principal: serde_json::Value,
    cancel: Cancel,
}
#[derive(Default)]
struct ReplicaFlow {
    pending: Vec<StateEntry>,
    scheduled: bool,
    in_flight: usize,
    queued: std::collections::VecDeque<Body>,
}
#[derive(Default)]
struct Connection {
    channel: Option<Arc<Channel>>,
    // The owner of a link change runs close/cancellation callbacks outside this lock.
    changing: bool,
}
struct Inner {
    host: Option<crate::host::WeakHost>,
    batch: Batch,
    options: RemoteOptions,
    principal: Mutex<serde_json::Value>,
    retry_disconnected: Option<Arc<dyn Fn() + Send + Sync>>,
    child_ready: AtomicBool,
    incarnation: [u8; 16],
    connection: Mutex<Connection>,
    epoch: AtomicU64,
    next_hold: AtomicU64,
    next_call: AtomicU64,
    holds: Mutex<BTreeMap<u64, Weak<Reference>>>,
    outgoing: Mutex<BTreeMap<u64, Outgoing>>,
    serving: Mutex<BTreeMap<u64, Serving>>,
    pending_routes: Mutex<BTreeMap<u64, PendingRoute>>,
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
            let peer = RemotePeer(inner);
            {
                let connection = lock(&peer.0.connection);
                if peer.0.epoch.load(Ordering::Acquire) != self.epoch {
                    return;
                }
                let Some((_, calls)) = connection.channel.as_ref().and_then(|c| c.peer()) else {
                    return;
                };
                *lock(&peer.0.window) = Arc::new(Semaphore::new(
                    calls.and_then(|n| usize::try_from(n).ok()).unwrap_or(65536),
                ));
            }
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
        if let Some(inner) = self.peer.upgrade() {
            RemotePeer(inner).disconnected(self.epoch);
        }
    }
}
struct Reference {
    id: u64,
    contract: Arc<Contract>,
    params: JsonValue,
    stamp: Option<(Box<JsonValue>, [u8; 16])>,
    peer: Weak<Inner>,
    state: NativeSource<RefState>,
    changed: watch::Sender<u64>,
    variant: Mutex<Option<String>>,
    members: Mutex<IndexMap<String, Source>>,
    decoders: Mutex<Vec<Decoder>>,
    released: AtomicBool,
    sent_epoch: AtomicU64,
    abort: Mutex<Option<AbortSubscription>>,
}
#[derive(Clone)]
pub struct RemoteReference(Arc<Reference>);
impl RemotePeer {
    pub(crate) fn revoke_forwarded(
        &self,
        selector: &serde_json::Value,
        matches: impl Fn(&serde_json::Value) -> bool,
    ) -> Result<()> {
        // The route may still be resolving when the caller loses access.
        let pending = lock(&self.0.pending_routes)
            .extract_if(.., |_, route| matches(&route.principal))
            .collect::<Vec<_>>();
        for (hold, route) in pending {
            route.cancel.cancel("revoked");
            if let Err(error) = self.send(Body::Gone {
                hold,
                error: wire_error(&crate::host::revoked().into())?,
            }) {
                (self.0.options.report)(error.to_string());
            }
        }
        let options = &self.0.options;
        if options.role != LinkRole::Route && !(options.role == LinkRole::Child && options.trusted)
        {
            return Ok(());
        }
        // A disconnected route still owns holds that would otherwise be retried.
        if let Ok(channel) = self.channel() {
            let selector = JsonValue::from_serde(selector.clone());
            if let Err(error) = channel.send(Message::new(Body::Revoke(selector))) {
                (options.report)(error.to_string());
                channel.close();
            }
        }
        let held: Vec<_> = lock(&self.0.holds)
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for held in held {
            if let Some((principal, _)) = &held.stamp
                && matches(&principal.to_serde()?)
            {
                RemoteReference(held).end(crate::host::revoked());
            }
        }
        Ok(())
    }

    pub(crate) fn close_revoked_child(&self, matches: impl Fn(&serde_json::Value) -> bool) {
        if self.0.options.role == LinkRole::Child && matches(&lock(&self.0.principal)) {
            self.close();
        }
    }

    pub fn connect(link: Arc<dyn Link>, options: RemoteOptions, batch: Batch) -> Result<Self> {
        let peer = Self::make(None, options, batch, None)?;
        peer.reconnect(link)?;
        Ok(peer)
    }
    pub fn serve(host: Host, link: Arc<dyn Link>, options: RemoteOptions) -> Result<Self> {
        let batch = host.batch();
        let peer = Self::make(Some(host), options, batch, None)?;
        peer.reconnect(link)?;
        Ok(peer)
    }
    /// Keep references and their caller stamps across replacement child processes.
    /// The supervisor closes the peer when it retires so the host can release it.
    pub fn supervised_child(
        host: Host,
        options: RemoteOptions,
        retry: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        if !options.trusted || options.role != LinkRole::Child {
            return Err(Error::Invalid(
                "supervised peers require a trusted child link".into(),
            ));
        }
        let batch = host.batch();
        let peer = Self::make(Some(host.clone()), options, batch, Some(Arc::new(retry)))?;
        host.keep_peer(peer.clone());
        Ok(peer)
    }

    pub(crate) fn same_peer(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    fn make(
        host: Option<Host>,
        options: RemoteOptions,
        batch: Batch,
        retry_disconnected: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self> {
        crate::data::principal_key(&options.principal)?;
        if options.call_window == 0 {
            return Err(Error::Invalid("call window is positive".into()));
        }
        let id = uuid::Uuid::parse_str(&crate::jobs::uuid_v7()?)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(Self(Arc::new(Inner {
            host: host.as_ref().map(Host::downgrade),
            batch,
            principal: Mutex::new(options.principal.clone()),
            child_ready: AtomicBool::new(retry_disconnected.is_none()),
            retry_disconnected,
            options,
            incarnation: *id.as_bytes(),
            connection: Mutex::default(),
            epoch: AtomicU64::new(0),
            next_hold: AtomicU64::new(0),
            next_call: AtomicU64::new(0),
            holds: Mutex::new(BTreeMap::new()),
            outgoing: Mutex::new(BTreeMap::new()),
            serving: Mutex::new(BTreeMap::new()),
            pending_routes: Mutex::new(BTreeMap::new()),
            incoming: Mutex::new(BTreeMap::new()),
            window: Mutex::new(Arc::new(Semaphore::new(0))),
            closed: AtomicBool::new(false),
            replica: Mutex::new(ReplicaFlow::default()),
        })))
    }

    pub fn connect_child(&self, link: Arc<dyn Link>, principal: serde_json::Value) -> Result<()> {
        if self.0.retry_disconnected.is_none() || self.channel().is_ok() {
            return Err(Error::Invalid(
                "expected a disconnected supervised child".into(),
            ));
        }
        crate::data::principal_key(&principal)?;
        self.0.child_ready.store(false, Ordering::Release);
        *lock(&self.0.principal) = principal;
        self.reconnect(link)
    }

    /// Child startup can call the parent before the child's objects are ready to bind.
    pub fn child_ready(&self) -> Result<()> {
        self.0.child_ready.store(true, Ordering::Release);
        let holds = lock(&self.0.holds)
            .values()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        for hold in holds {
            self.send_hold(&hold)?;
        }
        Ok(())
    }

    pub fn has_holds(&self) -> bool {
        lock(&self.0.holds)
            .values()
            .any(|reference| reference.strong_count() > 0)
    }

    /// Publish a startup failure without retiring the holds needed for Retry.
    pub fn fail_child(&self, error: ChordError) -> Result<()> {
        if self.0.retry_disconnected.is_none() || self.channel().is_ok() {
            return Err(Error::Invalid(
                "expected a disconnected supervised child".into(),
            ));
        }
        let holds = lock(&self.0.holds)
            .values()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        for hold in holds {
            hold.transition(RefState::Failed {
                cause: FailureCause {
                    kind: FailureKind::Broken,
                    error: error.clone(),
                },
                retry_at: None,
            })?;
        }
        Ok(())
    }

    pub fn reconnect(&self, link: Arc<dyn Link>) -> Result<()> {
        let (epoch, old) = {
            let mut connection = lock(&self.0.connection);
            if self.0.closed.load(Ordering::Acquire) {
                return Err(Error::Closed);
            }
            if connection.changing {
                return Err(ChordError::new("unavailable", "link change in progress").into());
            }
            connection.changing = true;
            let epoch = self.0.epoch.fetch_add(1, Ordering::AcqRel) + 1;
            (epoch, connection.channel.take())
        };
        if let Some(old) = old {
            old.close();
            self.dropped();
        }
        let report = self.0.options.report.clone();
        let prepared = Channel::prepare(
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
        );
        match prepared {
            Ok((channel, dispatch)) if self.finish_change(Some(channel.clone())) => {
                dispatch();
                Ok(())
            }
            Ok(_) => Err(Error::Closed),
            Err(error) => {
                self.finish_change(None);
                Err(error)
            }
        }
    }
    fn finish_change(&self, channel: Option<Arc<Channel>>) -> bool {
        let mut connection = lock(&self.0.connection);
        connection.changing = false;
        let closed = self.0.closed.load(Ordering::Acquire);
        if !closed && !channel.as_ref().is_some_and(|channel| channel.closed()) {
            connection.channel = channel;
            return true;
        }
        drop(connection);
        if let Some(channel) = channel {
            channel.close();
        }
        if closed {
            self.end_holds();
        }
        false
    }
    fn disconnected(&self, epoch: u64) {
        let old = {
            let mut connection = lock(&self.0.connection);
            if connection.changing
                || self.0.epoch.load(Ordering::Acquire) != epoch
                || connection.channel.is_none()
            {
                return;
            }
            connection.changing = true;
            self.0.epoch.fetch_add(1, Ordering::AcqRel);
            connection.channel.take()
        };
        drop(old);
        self.dropped();
        self.finish_change(None);
    }
    fn channel(&self) -> Result<Arc<Channel>> {
        self.channel_with_epoch().map(|(channel, _)| channel)
    }
    fn channel_with_epoch(&self) -> Result<(Arc<Channel>, u64)> {
        let connection = lock(&self.0.connection);
        connection
            .channel
            .clone()
            .filter(|c| !c.closed())
            .map(|channel| (channel, self.0.epoch.load(Ordering::Acquire)))
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
        let channel = {
            let mut connection = lock(&self.0.connection);
            if self.0.closed.swap(true, Ordering::AcqRel) {
                return;
            }
            if connection.changing {
                // An in-progress change will observe the terminal close and end the holds.
                None
            } else {
                connection.changing = true;
                self.0.epoch.fetch_add(1, Ordering::AcqRel);
                Some(connection.channel.take())
            }
        };
        if self.0.retry_disconnected.is_some()
            && let Some(host) = self
                .0
                .host
                .as_ref()
                .and_then(crate::host::WeakHost::upgrade)
        {
            host.forget_peer(self);
        }
        let Some(channel) = channel else { return };
        if let Some(channel) = channel {
            channel.close();
        }
        self.dropped();
        self.finish_change(None);
    }
    fn end_holds(&self) {
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
    pub async fn fence(&self) -> Result<()> {
        self.channel()?.fence().await
    }
    pub fn hold(
        &self,
        contract: Arc<Contract>,
        params: JsonValue,
        context: &Context,
    ) -> Result<RemoteReference> {
        self.hold_with_stamp(contract, params, None, context)
    }
    /// Forward a link-stamped caller over a trusted link to a child.
    pub fn hold_as(
        &self,
        contract: Arc<Contract>,
        params: JsonValue,
        caller: Caller,
        context: &Context,
    ) -> Result<RemoteReference> {
        if !self.0.options.trusted || self.0.options.role != LinkRole::Child {
            return Err(ChordError::new(
                "forbidden",
                "caller stamping requires a trusted child link",
            )
            .into());
        }
        let principal = caller
            .principal
            .ok_or_else(|| ChordError::new("forbidden", "remote reference requires a principal"))?;
        crate::data::principal_key(&principal)?;
        let incarnation = uuid::Uuid::parse_str(&caller.incarnation)
            .map_err(|_| Error::Invalid("invalid caller incarnation".into()))?;
        let stamp = (
            Box::new(JsonValue::from_serde(principal)),
            *incarnation.as_bytes(),
        );
        self.hold_with_stamp(contract, params, Some(stamp), context)
    }
    fn hold_with_stamp(
        &self,
        contract: Arc<Contract>,
        params: JsonValue,
        stamp: Option<(Box<JsonValue>, [u8; 16])>,
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
            stamp,
            peer: Arc::downgrade(&self.0),
            state: NativeSource::new(self.0.batch.clone(), RefState::Loading, |a, b| a == b),
            changed,
            variant: Mutex::new(None),
            members: Mutex::new(IndexMap::new()),
            decoders: Mutex::new(Vec::new()),
            released: AtomicBool::new(false),
            sent_epoch: AtomicU64::new(0),
            abort: Mutex::new(None),
        }));
        {
            let mut holds = lock(&self.0.holds);
            if self.0.closed.load(Ordering::Acquire) {
                return Err(Error::Closed);
            }
            holds.insert(id, Arc::downgrade(&held.0));
        }
        if let Some(signal) = context.abort_signal() {
            let weak = Arc::downgrade(&held.0);
            *lock(&held.0.abort) = Some(signal.subscribe(move |_| {
                if let Some(held) = weak.upgrade() {
                    RemoteReference(held).release();
                }
            }));
        }
        match self.channel() {
            Ok(channel) if channel.peer().is_some() => {
                if let Err(error) = self.send_hold(&held.0)
                    && (self.0.retry_disconnected.is_none() || !channel.closed())
                {
                    return Err(error);
                }
            }
            Err(error) if self.0.retry_disconnected.is_none() => return Err(error),
            _ => {}
        }
        Ok(held)
    }
    fn send_hold(&self, held: &Reference) -> Result<()> {
        if held.released.load(Ordering::Acquire) || !self.0.child_ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let (kind, params) = if held.contract.kind() == crate::contracts::ContractKind::Remote {
            let address = held.contract.host_address(&held.params)?;
            (address.kind, address.params)
        } else {
            ("server".into(), JsonValue::Object(IndexMap::new()))
        };
        let (channel, epoch) = self.channel_with_epoch()?;
        if channel.peer().is_none() || held.sent_epoch.fetch_max(epoch, Ordering::AcqRel) >= epoch {
            return Ok(());
        }
        channel.send(Message::new(Body::Hold {
            hold: held.id,
            type_name: held.contract.name().into(),
            version: held.contract.version(),
            fingerprint: fingerprint(held.contract.fingerprint())?,
            params: Part::Text(held.params.to_json()?),
            address_kind: kind,
            address_params: Box::new(params),
            principal: held.stamp.as_ref().map(|(principal, _)| principal.clone()),
            incarnation: held.stamp.as_ref().map(|(_, incarnation)| *incarnation),
            chain: None,
            limit: None,
        }))?;
        if held.released.load(Ordering::Acquire) {
            // Release may have crossed the Hold while the child was becoming ready.
            channel.send(Message::new(Body::Release { hold: held.id }))?;
        }
        Ok(())
    }
    fn dropped(&self) {
        let pending_routes = std::mem::take(&mut *lock(&self.0.pending_routes));
        for (_, route) in pending_routes {
            route.cancel.cancel("link dropped");
        }
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
        let names = held.contract.variant_member_names(Some(&variant))?.state;
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
        let names = held.contract.variant_member_names(Some(&variant))?.state;
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
        let changed = self.changed.clone();
        self.state.set_with(state, &Context::background(), move || {
            changed.send_modify(|n| *n = n.wrapping_add(1));
        })
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
        let names = self.0.contract.variant_member_names(Some(&variant))?;
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
        self.retry_with_hops(16)
    }
    fn retry_with_hops(&self, hops: u64) -> Result<()> {
        let peer = RemotePeer(self.0.peer.upgrade().ok_or(Error::Closed)?);
        if let Some(retry) = &peer.0.retry_disconnected
            && peer.channel().is_err()
            && !peer.0.closed.load(Ordering::Acquire)
        {
            retry();
            return Ok(());
        }
        peer.send(Body::Retry {
            hold: self.0.id,
            hops,
        })
    }
    pub fn release(&self) {
        self.end(ChordError::new("released", "reference released"));
    }

    fn end(&self, error: ChordError) {
        if !self.0.released.swap(true, Ordering::AcqRel) {
            lock(&self.0.abort).take();
            let changed = self.0.changed.clone();
            let _ = self.0.state.set_with(
                RefState::Ended {
                    error: error.clone(),
                },
                &Context::background(),
                move || changed.send_modify(|n| *n = n.wrapping_add(1)),
            );
            for source in lock(&self.0.members).values() {
                source.detach("released");
            }
            if let Some(peer) = self.0.peer.upgrade() {
                RemotePeer(peer).release(self.0.id, error);
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
            RemotePeer(peer).release(self.id, ChordError::new("released", "reference released"));
        }
    }
}
impl RemotePeer {
    fn release(&self, id: u64, error: ChordError) {
        lock(&self.0.holds).remove(&id);
        let _ = self.send(Body::Release { hold: id });
        let ids = lock(&self.0.outgoing)
            .iter()
            .filter_map(|(call, c)| (c.hold == id).then_some(*call))
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(call) = lock(&self.0.outgoing).remove(&id) {
                let _ = call.reply.send(Err(error.clone().into()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{HostOptions, create_host};

    #[tokio::test]
    async fn host_keeps_supervised_peer_until_terminal_close() -> Result<()> {
        let context = Context::background();
        let host = create_host(HostOptions::default())?;
        let mut options = RemoteOptions::new(serde_json::json!("worker"));
        options.role = LinkRole::Child;
        options.trusted = true;
        let retiring = RemotePeer::supervised_child(host.clone(), options.clone(), || {})?;
        let reconnecting = RemotePeer::supervised_child(host.clone(), options, || {})?;

        let retired = Arc::downgrade(&retiring.0);
        retiring.close();
        retiring.close();
        drop(retiring);
        assert!(retired.upgrade().is_none());

        let (link, _) = crate::link::memory_link_pair();
        host.close(&context).await?;
        assert!(matches!(reconnecting.reconnect(link), Err(Error::Closed)));
        reconnecting.close();
        Ok(())
    }
}
