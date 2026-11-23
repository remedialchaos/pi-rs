use super::{
    Definition, DocumentStore, KeyedBy, Line, LineLease, SealToken, Stored,
    persistence::{Loaded, Plan},
};
use crate::{
    Error, Result,
    context::{Context, lock},
    delta::{Change, Op, Prepared, Revision, Segment, Tracker},
    json::{JsonString, JsonValue},
    state::{Batch, Source},
    watch::Watch,
};
use indexmap::IndexMap;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin as FuturePin,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    sync::{Notify, watch},
    time::Instant,
};
type Work<T> = FuturePin<Box<dyn Future<Output = Result<T>> + Send>>;
const RETENTION: Duration = Duration::from_secs(600);
const GRANULE: Duration = Duration::from_secs(1);
fn released(message: impl Into<String>) -> Error {
    Error::Chord(Box::new(crate::data::ChordError {
        code: "released".into(),
        reason: Some("sealed".into()),
        message: message.into(),
        details: None,
    }))
}
#[derive(Clone)]
pub enum DocumentKey {
    String(JsonString),
    Address(JsonValue),
}
impl From<&str> for DocumentKey {
    fn from(v: &str) -> Self {
        Self::String(v.into())
    }
}
impl From<String> for DocumentKey {
    fn from(v: String) -> Self {
        Self::String(v.as_str().into())
    }
}
impl From<JsonString> for DocumentKey {
    fn from(v: JsonString) -> Self {
        Self::String(v)
    }
}
struct Resident {
    tracker: Tracker,
    stored: Option<Stored>,
    version: u64,
    touched: Instant,
}
struct SlotState {
    definition: Arc<Definition>,
    hydrated: bool,
    loaded: bool,
    initial: Option<Revision>,
    singleton: Option<Source>,
    residents: IndexMap<String, Resident>,
    pins: HashMap<String, Weak<PinInner>>,
    loads: HashMap<String, Arc<Load>>,
}
struct Slot {
    owner: Weak<Inner>,
    state: Mutex<SlotState>,
}
struct Load {
    finished: watch::Sender<Option<std::result::Result<(), String>>>,
    tokens: Mutex<Vec<SealToken>>,
    refused: Mutex<Vec<SealToken>>,
}
struct OwnerState {
    status: u8,
    failure: Option<String>,
    slots: IndexMap<String, Arc<Slot>>,
    holder: Weak<TxInner>,
    pins: Vec<Pin>,
}
struct Inner {
    label: String,
    batch: Batch,
    store: Option<DocumentStore>,
    state: Mutex<OwnerState>,
    line: Line,
    own_token: SealToken,
    notify: Arc<Notify>,
    report: Arc<dyn Fn(String) + Send + Sync>,
}
#[derive(Clone)]
pub struct Documents(Arc<Inner>);
impl Documents {
    pub(crate) async fn lock_host_data(&self) -> Result<LineLease> {
        self.0.line.acquire().await
    }

    pub(crate) fn prepare_host_data(
        &self,
        connection: &rusqlite::Connection,
        address: &crate::contracts::Address,
    ) -> Result<Vec<crate::store::HostDataAdoption>> {
        let mut changes: Vec<crate::store::HostDataAdoption> = Vec::new();
        let Some(store) = &self.0.store else {
            return Ok(changes);
        };
        let slots = lock(&self.0.state)
            .slots
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for slot in slots {
            let (name, prior, pin) = {
                let state = lock(&slot.state);
                if !state.hydrated
                    || !state.definition.persist
                    || !matches!(&state.definition.keyed, KeyedBy::Host(host) if host.kind == address.kind)
                {
                    continue;
                }
                let prior = state
                    .residents
                    .get(&address.key)
                    .and_then(|r| r.stored.clone());
                let pin = state.pins.get(&address.key).and_then(Weak::upgrade);
                if prior.is_none() && pin.is_none() {
                    continue;
                }
                (state.definition.name.clone(), prior, pin)
            };
            let loaded = store.read_in(connection, &name, &address.key, true)?;
            if loaded.as_ref().map(|r| &r.stored) == prior.as_ref() {
                continue;
            }
            let retired = loaded.is_none();
            let resident = match loaded.map(|r| slot.decode(r)).transpose() {
                Ok(resident) => resident,
                Err(error) => {
                    let owner = self.clone();
                    changes.push(Box::new(move |_| {
                        owner.fail(&error);
                        Ok(())
                    }));
                    break;
                }
            };
            let key = address.key.clone();
            changes.push(Box::new(move |context| {
                let mut state = lock(&slot.state);
                let value = if let Some(resident) = resident {
                    let value = resident.tracker.value();
                    state.residents.insert(key.clone(), resident);
                    Some(value)
                } else {
                    state.residents.shift_remove(&key);
                    pin.as_ref().map(|pin| lock(&pin.initial).clone())
                };
                drop(state);
                if let (Some(pin), Some(value)) = (pin, value) {
                    pin.source.address_revision(
                        value.clone(),
                        vec![Op::Replace(value.to_json())],
                        retired,
                        context,
                    )?;
                }
                Ok(())
            }));
        }
        Ok(changes)
    }
    pub fn new(
        label: impl Into<String>,
        batch: Batch,
        store: Option<DocumentStore>,
        report: impl Fn(String) + Send + Sync + 'static,
    ) -> Self {
        // An integrated store owns the publication batch for both documents and SQL reads.
        let batch = store
            .as_ref()
            .and_then(DocumentStore::batch)
            .unwrap_or(batch);
        let docs = Self(Arc::new(Inner {
            label: label.into(),
            batch,
            store,
            state: Mutex::new(OwnerState {
                status: 0,
                failure: None,
                slots: IndexMap::new(),
                holder: Weak::new(),
                pins: Vec::new(),
            }),
            line: Line::default(),
            own_token: SealToken::default(),
            notify: Arc::new(Notify::new()),
            report: Arc::new(report),
        }));
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&docs.0);
            let notify = docs.0.notify.clone();
            runtime.spawn(async move{loop{
            let notified=notify.notified();let Some(inner)=weak.upgrade()else{break};let docs=Documents(inner);if lock(&docs.0.state).status==2{break}
            let deadline=docs.deadline();drop(docs);
            match deadline{Some(deadline)=>tokio::select!{_=tokio::time::sleep_until(deadline)=>{},_=notified=>{continue}},None=>{notified.await;continue}}
            let Some(inner)=weak.upgrade()else{break};let docs=Documents(inner);if let Err(error)=docs.evict().await{(docs.0.report)(error.to_string())}
        }});
        }
        docs
    }
    fn alive(&self) -> Result<()> {
        let state = lock(&self.0.state);
        if let Some(failure) = &state.failure {
            Err(Error::Chord(Box::new(crate::data::ChordError::new(
                "broken",
                failure.clone(),
            ))))
        } else if state.status == 2 {
            Err(released(format!("{}: ended", self.0.label)))
        } else {
            Ok(())
        }
    }
    fn fail(&self, error: &Error) {
        lock(&self.0.state).failure = Some(error.to_string());
        (self.0.report)(error.to_string());
        self.end();
    }
    fn admit(&self, token: &SealToken) -> Result<()> {
        self.alive()?;
        if lock(&self.0.state).status != 0 || token.sealed() {
            Err(released(format!("{}: sealed", self.0.label)))
        } else {
            Ok(())
        }
    }
    pub fn attempt(&self) -> Attempt {
        Attempt {
            docs: self.clone(),
            token: SealToken::default(),
            state: Mutex::new(AttemptState {
                declarations: IndexMap::new(),
                pins: Vec::new(),
                open: true,
            }),
        }
    }
    pub fn token(&self) -> &SealToken {
        &self.0.own_token
    }
    pub fn batch(&self) -> &Batch {
        &self.0.batch
    }
    pub fn fence(&self, token: &SealToken) {
        token.seal();
        let holder = lock(&self.0.state).holder.upgrade();
        if let Some(holder) = holder
            && holder.token.same(token)
        {
            Transaction(holder).cut();
        }
    }
    pub fn seal(&self) {
        let mut state = lock(&self.0.state);
        if state.status != 0 {
            return;
        }
        state.status = 1;
        let holder = state.holder.upgrade();
        drop(state);
        self.0.own_token.seal();
        if let Some(holder) = holder {
            Transaction(holder).cut()
        }
    }
    pub async fn settled(&self) -> Result<()> {
        self.0.line.acquire().await?.release();
        Ok(())
    }
    pub fn end(&self) {
        self.seal();
        let slots = {
            let mut state = lock(&self.0.state);
            state.status = 2;
            state.pins.clear();
            std::mem::take(&mut state.slots)
        };
        for slot in slots.into_values() {
            slot.end()
        }
        self.0.notify.notify_one();
    }
    pub async fn transaction<T, F, Fut>(
        &self,
        token: &SealToken,
        context: &Context,
        callback: F,
    ) -> Result<T>
    where
        F: FnOnce(Transaction) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.0.batch.assert_outside("tx")?;
        self.admit(token)?;
        let lease = self.0.line.acquire().await?;
        self.admit(token)?;
        let tx = Transaction(Arc::new(TxInner {
            owner: Arc::downgrade(&self.0),
            token: token.clone(),
            context: context.clone(),
            state: Mutex::new(TxState {
                phase: Phase::Open,
                lease: Some(lease),
                entries: IndexMap::new(),
                pending: 0,
                staged: Vec::new(),
            }),
        }));
        lock(&self.0.state).holder = Arc::downgrade(&tx.0);
        let mut finish = Finish {
            tx: tx.clone(),
            settled: false,
        };
        let result = match callback(tx.clone()).await {
            Ok(value) => tx.commit().await.map(|_| value),
            Err(error) => Err(error),
        };
        if result.is_err() {
            tx.discard();
            if context.is_aborted() {
                (self.0.report)("transaction_cancelled".into())
            }
        }
        finish.settle();
        result
    }
    fn deadline(&self) -> Option<Instant> {
        let slots = lock(&self.0.state)
            .slots
            .values()
            .cloned()
            .collect::<Vec<_>>();
        slots
            .into_iter()
            .filter_map(|slot| {
                let s = lock(&slot.state);
                if !s.definition.persist || matches!(s.definition.keyed, KeyedBy::Singleton) {
                    return None;
                }
                s.residents
                    .iter()
                    .filter(|(k, _)| s.pins.get(*k).and_then(Weak::upgrade).is_none())
                    .map(|(_, r)| r.touched + RETENTION)
                    .min()
            })
            .min()
    }
    pub async fn evict(&self) -> Result<usize> {
        let lease = self.0.line.acquire().await?;
        self.alive()?;
        let mut count = 0;
        let slots = lock(&self.0.state)
            .slots
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for slot in slots {
            let mut s = lock(&slot.state);
            if !s.definition.persist || matches!(s.definition.keyed, KeyedBy::Singleton) {
                continue;
            }
            let now = Instant::now();
            let keys = s
                .residents
                .iter()
                .filter(|(key, r)| {
                    now.duration_since(r.touched) >= RETENTION
                        && s.pins.get(*key).and_then(Weak::upgrade).is_none()
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            for key in keys {
                s.residents.shift_remove(&key);
                count += 1;
            }
        }
        lease.release();
        Ok(count)
    }
    pub fn resident_count(&self) -> usize {
        lock(&self.0.state)
            .slots
            .values()
            .map(|s| lock(&s.state).residents.len())
            .sum()
    }
}
struct AttemptState {
    declarations: IndexMap<String, Arc<Definition>>,
    pins: Vec<Pin>,
    open: bool,
}
pub struct Attempt {
    docs: Documents,
    token: SealToken,
    state: Mutex<AttemptState>,
}
impl Attempt {
    pub fn token(&self) -> &SealToken {
        &self.token
    }
    pub fn declare(&self, definition: Arc<Definition>) -> Result<Handle> {
        self.docs.admit(&self.token)?;
        let mut attempt = lock(&self.state);
        if !attempt.open {
            return Err(released("factory settled"));
        }
        let mut owner = lock(&self.docs.0.state);
        let slot = if let Some(slot) = owner.slots.get(&definition.name) {
            lock(&slot.state).definition.compatible(&definition)?;
            slot.clone()
        } else {
            let slot = Arc::new(Slot {
                owner: Arc::downgrade(&self.docs.0),
                state: Mutex::new(SlotState {
                    definition: definition.clone(),
                    hydrated: false,
                    loaded: false,
                    initial: None,
                    singleton: None,
                    residents: IndexMap::new(),
                    pins: HashMap::new(),
                    loads: HashMap::new(),
                }),
            });
            owner.slots.insert(definition.name.clone(), slot.clone());
            slot
        };
        if let Some(previous) = attempt.declarations.get(&definition.name) {
            previous.compatible(&definition)?;
        }
        attempt
            .declarations
            .insert(definition.name.clone(), definition.clone());
        Ok(Handle {
            slot,
            version: definition.version,
            token: self.token.clone(),
        })
    }
    pub fn at(&self, handle: &Handle, key: impl Into<DocumentKey>) -> Result<Pin> {
        let mut state = lock(&self.state);
        if !state.open {
            return Err(Error::Invalid("at() is factory-only".into()));
        }
        handle.same_owner(&self.docs)?;
        let key = handle.slot.normalize(Some(key.into()))?;
        let pin = handle.slot.pin(key)?;
        state.pins.push(pin.clone());
        Ok(pin)
    }
    pub async fn activate(&self, context: &Context) -> Result<()> {
        self.docs.admit(&self.token)?;
        let (declarations, pins) = {
            let mut state = lock(&self.state);
            if !state.open {
                return Err(released("factory settled"));
            }
            state.open = false;
            (state.declarations.clone(), std::mem::take(&mut state.pins))
        };
        let lease = self.docs.0.line.acquire().await?;
        self.docs.admit(&self.token)?;
        let slots = lock(&self.docs.0.state).slots.clone();
        // Stage migrations before mutating any slot. A failing migration preserves the previous revision.
        let mut stages = Vec::new();
        for (name, definition) in &declarations {
            let slot = slots
                .get(name)
                .ok_or_else(|| Error::Invalid("missing declaration".into()))?;
            let stage = slot.hydration(definition.clone()).inspect_err(|error| {
                if matches!(error, Error::Chord(cause) if cause.code == "broken") {
                    self.docs.fail(error);
                }
            })?;
            stages.push((slot.clone(), stage));
        }
        let batch = self.docs.0.batch.clone();
        batch.commit(context, || {
            for (slot, stage) in stages {
                slot.hydrate(stage, context)?;
            }
            Ok(())
        })?;
        for (name, slot) in slots {
            if !declarations.contains_key(&name) {
                lock(&self.docs.0.state).slots.shift_remove(&name);
                slot.end();
            }
        }
        {
            let old = std::mem::replace(&mut lock(&self.docs.0.state).pins, pins.clone());
            drop(old);
        }
        let slots = lock(&self.docs.0.state)
            .slots
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for slot in slots {
            let singleton = matches!(lock(&slot.state).definition.keyed, KeyedBy::Singleton);
            if singleton {
                slot.load_held("", &self.token, true).await?;
            }
        }
        for pin in pins {
            pin.0.slot.load_held(&pin.0.key, &self.token, true).await?;
        }
        lease.release();
        Ok(())
    }
}
struct Hydration {
    definition: Arc<Definition>,
    initial: Option<Revision>,
    residents: Vec<(String, Tracker, u64)>,
    pin_initials: Vec<(Arc<PinInner>, Revision)>,
    changed: bool,
}
impl Slot {
    fn owner(&self) -> Result<Documents> {
        let docs = Documents(
            self.owner
                .upgrade()
                .ok_or_else(|| released("owner ended"))?,
        );
        docs.alive()?;
        Ok(docs)
    }
    fn normalize(&self, key: Option<DocumentKey>) -> Result<String> {
        let s = lock(&self.state);
        let key = match (&s.definition.keyed, key) {
            (KeyedBy::Singleton, None) => String::new(),
            (KeyedBy::String, Some(DocumentKey::String(s))) => String::from_utf16_lossy(&s.0),
            (KeyedBy::Host(host), Some(DocumentKey::Address(value))) => host
                .key_of_address(&value)?
                .ok_or_else(|| Error::Invalid("invalid/args: wrong document host kind".into()))?,
            _ => return Err(Error::Invalid("invalid/args: document key shape".into())),
        };
        if key.contains('\0') {
            return Err(Error::Invalid("invalid/args: NUL in document key".into()));
        }
        Ok(key)
    }
    fn hydration(&self, definition: Arc<Definition>) -> Result<Hydration> {
        let s = lock(&self.state);
        s.definition.compatible(&definition)?;
        let changed = !s.hydrated || s.definition.version != definition.version;
        let initial = if matches!(definition.keyed, KeyedBy::Singleton) {
            if changed {
                Some(Revision::from_json(definition.initial()?)?)
            } else {
                s.initial.clone()
            }
        } else {
            None
        };
        let mut residents = Vec::new();
        let mut pin_initials = Vec::new();
        if changed {
            for (key, resident) in &s.residents {
                if matches!(definition.keyed, KeyedBy::Singleton)
                    || s.pins.get(key).and_then(Weak::upgrade).is_some()
                {
                    let migrated = definition
                        .migrate(resident.tracker.value().to_json(), resident.version)
                        .and_then(|value| Tracker::new(value).map_err(Error::from))
                        .map_err(|error| {
                            if matches!(definition.keyed, KeyedBy::Singleton) {
                                error
                            } else {
                                Error::Chord(Box::new(crate::data::ChordError::new(
                                    "broken",
                                    error.to_string(),
                                )))
                            }
                        })?;
                    residents.push((key.clone(), migrated, definition.version));
                }
            }
            for pin in s.pins.values().filter_map(Weak::upgrade) {
                pin_initials.push((pin, Revision::from_json(definition.initial()?)?));
            }
        }
        Ok(Hydration {
            definition,
            initial,
            residents,
            pin_initials,
            changed,
        })
    }
    fn hydrate(&self, stage: Hydration, context: &Context) -> Result<()> {
        let docs = self.owner()?;
        let mut s = lock(&self.state);
        s.definition = stage.definition.clone();
        s.initial = stage.initial;
        s.hydrated = true;
        for (key, tracker, version) in stage.residents {
            if let Some(resident) = s.residents.get_mut(&key) {
                resident.tracker = tracker;
                resident.version = version;
            }
        }
        if matches!(s.definition.keyed, KeyedBy::Singleton) && s.singleton.is_none() {
            let source = Source::framed(docs.0.batch.clone(), s.definition.name.clone(), |_| {});
            if let Some(initial) = s.initial.clone() {
                source.address_revision(
                    initial.clone(),
                    vec![Op::Replace(initial.to_json())],
                    false,
                    context,
                )?;
            }
            s.singleton = Some(source);
        }
        let single = s.singleton.clone();
        let value = s
            .residents
            .get("")
            .map(|r| r.tracker.value())
            .or_else(|| s.initial.clone());
        drop(s);
        if stage.changed
            && let (Some(source), Some(value)) = (single, value)
        {
            source.address_revision(
                value.clone(),
                vec![Op::Replace(value.to_json())],
                false,
                context,
            )?;
        }
        for (pin, initial) in stage.pin_initials {
            *lock(&pin.initial) = initial.clone();
            let value = self.current(&pin.key)?.unwrap_or(initial);
            pin.source.address_revision(
                value.clone(),
                vec![Op::Replace(value.to_json())],
                false,
                context,
            )?;
        }
        Ok(())
    }
    fn current(&self, key: &str) -> Result<Option<Revision>> {
        let docs = self.owner()?;
        let mut s = lock(&self.state);
        if !s.hydrated {
            return Err(Error::Invalid(format!(
                "not_ready: {}: read before activation",
                s.definition.name
            )));
        }
        let definition = s.definition.clone();
        if let Some(resident) = s.residents.get_mut(key) {
            if resident.version != definition.version {
                let migrated = definition
                    .migrate(resident.tracker.value().to_json(), resident.version)
                    .and_then(|value| Tracker::new(value).map_err(Error::from));
                match migrated {
                    Ok(tracker) => resident.tracker = tracker,
                    Err(error) => {
                        drop(s);
                        docs.fail(&error);
                        return Err(error);
                    }
                }
                resident.version = definition.version;
            }
            if resident.touched.elapsed() >= GRANULE {
                resident.touched = Instant::now();
            }
            return Ok(Some(resident.tracker.value()));
        }
        Ok(None)
    }
    fn decode(&self, loaded: Loaded) -> Result<Resident> {
        let def = lock(&self.state).definition.clone();
        if matches!(def.keyed, KeyedBy::Host(_)) != loaded.host.is_some() {
            return Err(Error::Invalid(format!(
                "{}: stored host key kind differs from declaration",
                def.name
            )));
        }
        def.readable(loaded.stored.version, &loaded.fingerprint)?;
        let value = if loaded.stored.version == def.version {
            loaded.value
        } else {
            def.migrate(loaded.value, loaded.stored.version)?
        };
        Ok(Resident {
            tracker: Tracker::new(value)?,
            stored: Some(loaded.stored),
            version: def.version,
            touched: Instant::now(),
        })
    }
    async fn load_held(&self, key: &str, token: &SealToken, cuttable: bool) -> Result<()> {
        let docs = self.owner()?;
        let (def, loaded, resident) = {
            let s = lock(&self.state);
            (
                s.definition.clone(),
                s.loaded,
                s.residents.contains_key(key),
            )
        };
        if !def.persist || resident || matches!(def.keyed, KeyedBy::Singleton) && loaded {
            return Ok(());
        }
        let store = docs
            .0
            .store
            .as_ref()
            .ok_or_else(|| Error::Invalid("persisted document requires a store".into()))?;
        let loaded = store.read(def.name.clone(), key.to_string(), true).await?;
        if cuttable && token.sealed() {
            return Err(released("load fenced"));
        }
        let resident = loaded
            .map(|loaded| self.decode(loaded))
            .transpose()
            .inspect_err(|error| docs.fail(error))?;
        let source = {
            let mut s = lock(&self.state);
            if let Some(resident) = resident {
                s.residents.entry(key.into()).or_insert(resident);
            }
            if matches!(def.keyed, KeyedBy::Singleton) {
                s.loaded = true;
                s.singleton.clone()
            } else {
                s.pins
                    .get(key)
                    .and_then(Weak::upgrade)
                    .map(|p| p.source.clone())
            }
        };
        if let Some(source) = source {
            let value = self
                .current(key)?
                .or_else(|| lock(&self.state).initial.clone())
                .or_else(|| {
                    lock(&self.state)
                        .pins
                        .get(key)
                        .and_then(Weak::upgrade)
                        .map(|p| lock(&p.initial).clone())
                });
            if let Some(value) = value {
                source.address_revision(
                    value.clone(),
                    vec![Op::Replace(value.to_json())],
                    false,
                    &Context::background(),
                )?;
            }
        }
        docs.0.notify.notify_one();
        Ok(())
    }
    fn start_load(self: &Arc<Self>, key: String, token: SealToken) -> Result<Arc<Load>> {
        let docs = self.owner()?;
        docs.admit(&token)?;
        let mut s = lock(&self.state);
        if let Some(load) = s.loads.get(&key) {
            if token.sealed() {
                return Err(released("load fenced"));
            }
            lock(&load.tokens).push(token);
            return Ok(load.clone());
        }
        docs.admit(&token)?;
        let (finished, _) = watch::channel(None);
        let load = Arc::new(Load {
            finished,
            tokens: Mutex::new(vec![token]),
            refused: Mutex::new(Vec::new()),
        });
        s.loads.insert(key.clone(), load.clone());
        drop(s);
        let lease = docs.0.line.acquire();
        let slot = self.clone();
        let loaded = load.clone();
        tokio::spawn(async move {
            let result = async {
                let _lease = lease.await?;
                docs.alive()?;
                let admitted = {
                    let tokens = lock(&loaded.tokens);
                    *lock(&loaded.refused) = tokens
                        .iter()
                        .filter(|token| token.sealed())
                        .cloned()
                        .collect();
                    tokens
                        .iter()
                        .find(|token| !token.sealed())
                        .cloned()
                        .ok_or_else(|| released("load fenced"))?
                };
                slot.load_held(&key, &admitted, false).await
            }
            .await;
            let result = result.map_err(|error: Error| error.to_string());
            lock(&slot.state).loads.remove(&key);
            loaded.finished.send_replace(Some(result));
        });
        Ok(load)
    }
    fn pin(self: &Arc<Self>, key: String) -> Result<Pin> {
        let docs = self.owner()?;
        let mut s = lock(&self.state);
        if let Some(pin) = s.pins.get(&key).and_then(Weak::upgrade) {
            return Ok(Pin(pin));
        }
        let initial = Revision::from_json(s.definition.initial()?)?;
        let source = Source::framed(docs.0.batch.clone(), s.definition.name.clone(), |_| {});
        let ready = !s.definition.persist || s.residents.contains_key(&key);
        let value = s
            .residents
            .get(&key)
            .map(|r| r.tracker.value())
            .unwrap_or_else(|| initial.clone());
        if ready {
            source.address_revision(
                value.clone(),
                vec![Op::Replace(value.to_json())],
                false,
                &Context::background(),
            )?;
        }
        let pin = Arc::new(PinInner {
            slot: self.clone(),
            key: key.clone(),
            initial: Mutex::new(initial),
            source,
        });
        s.pins.insert(key, Arc::downgrade(&pin));
        Ok(Pin(pin))
    }
    fn end(&self) {
        let mut s = lock(&self.state);
        if let Some(source) = s.singleton.take() {
            source.detach("ended")
        }
        for pin in s.pins.values().filter_map(Weak::upgrade) {
            pin.source.detach("ended")
        }
        s.residents.clear();
        s.initial = None;
    }
}
struct PinInner {
    slot: Arc<Slot>,
    key: String,
    initial: Mutex<Revision>,
    source: Source,
}
#[derive(Clone)]
pub struct Pin(Arc<PinInner>);
impl Pin {
    pub fn source(&self) -> Source {
        self.0.source.clone()
    }
    pub fn value(&self) -> Result<Revision> {
        self.0.source.value()
    }
}
impl Drop for PinInner {
    fn drop(&mut self) {
        let mut s = lock(&self.slot.state);
        s.pins.remove(&self.key);
        if let Some(r) = s.residents.get_mut(&self.key) {
            r.touched = Instant::now();
        }
        drop(s);
        if let Some(owner) = self.slot.owner.upgrade() {
            owner.notify.notify_one();
        }
    }
}
#[derive(Clone)]
pub struct Handle {
    slot: Arc<Slot>,
    version: u64,
    token: SealToken,
}
impl Handle {
    pub fn token(&self) -> &SealToken {
        &self.token
    }
    fn same_owner(&self, docs: &Documents) -> Result<()> {
        if !self.slot.owner.ptr_eq(&Arc::downgrade(&docs.0)) {
            return Err(Error::Invalid(
                "invalid: document belongs to another owner".into(),
            ));
        }
        Ok(())
    }
    fn check(&self) -> Result<Documents> {
        let docs = self.slot.owner()?;
        let s = lock(&self.slot.state);
        if self.version < s.definition.version {
            return Err(released("document handle version is old"));
        }
        if !s.hydrated || self.version > s.definition.version {
            return Err(Error::Invalid(format!(
                "not_ready: {}: read before activation",
                s.definition.name
            )));
        }
        Ok(docs)
    }
    pub fn value(&self) -> Result<Revision> {
        self.check()?;
        let s = lock(&self.slot.state);
        if !matches!(s.definition.keyed, KeyedBy::Singleton) {
            return Err(Error::Invalid("key required".into()));
        }
        if s.definition.persist && !s.loaded {
            return Err(Error::Invalid("not_ready: read before load".into()));
        }
        let initial = s.initial.clone();
        drop(s);
        self.slot
            .current("")?
            .or(initial)
            .ok_or_else(|| Error::Invalid("not_ready".into()))
    }
    pub fn source(&self) -> Result<Source> {
        self.check()?;
        lock(&self.slot.state)
            .singleton
            .clone()
            .ok_or_else(|| Error::Invalid("singleton source required".into()))
    }
    pub async fn watch(&self, context: &Context) -> Result<Watch> {
        self.value()?;
        self.source()?.open_watch(context)
    }
    pub async fn get(
        &self,
        key: impl Into<DocumentKey>,
        context: &Context,
    ) -> Result<Option<Revision>> {
        self.check()?;
        let key = self.slot.normalize(Some(key.into()))?;
        if let Some(value) = self.slot.current(&key)? {
            return Ok(Some(value));
        }
        if !lock(&self.slot.state).definition.persist {
            return Ok(None);
        }
        let load = self.slot.start_load(key.clone(), self.token.clone())?;
        let mut receive = load.finished.subscribe();
        context
            .await_with(async move {
                loop {
                    let result = receive.borrow_and_update().clone();
                    if let Some(result) = result {
                        return result.map_err(Error::Invalid);
                    }
                    receive.changed().await.map_err(|_| Error::Closed)?;
                }
            })
            .await
            .map_err(|e| Error::Invalid(e.to_string()))??;
        if lock(&load.refused)
            .iter()
            .any(|token| token.same(&self.token))
        {
            return Err(released("load fenced before admission"));
        }
        self.slot.current(&key)
    }
    pub async fn watch_key(
        &self,
        key: impl Into<DocumentKey>,
        context: &Context,
    ) -> Result<Option<Watch>> {
        let key = key.into();
        if self.get(key.clone(), context).await?.is_none() {
            return Ok(None);
        }
        let key = self.slot.normalize(Some(key))?;
        let pin = self.slot.pin(key)?;
        let watch = pin.source().open_watch(context)?;
        let closed = watch.clone();
        tokio::spawn(async move {
            closed.closed().await;
            drop(pin);
        });
        Ok(Some(watch))
    }
    /// Keys as the handle accepts them. Host-keyed documents yield decoded addresses, not storage keys.
    pub async fn handle_keys(&self, context: &Context) -> Result<Vec<DocumentKey>> {
        let keys = self.keys(context).await?;
        let definition = lock(&self.slot.state).definition.clone();
        keys.into_iter()
            .map(|key| match &definition.keyed {
                KeyedBy::Host(host) => {
                    let address = host.address_of_key(&key)?;
                    Ok(DocumentKey::Address(JsonValue::Object(IndexMap::from([
                        ("kind".into(), JsonValue::String(address.kind.into())),
                        ("params".into(), address.params),
                        ("key".into(), JsonValue::String(address.key.into())),
                    ]))))
                }
                _ => Ok(DocumentKey::String(key.into())),
            })
            .collect()
    }
    /// Raw storage keys, in code point order. Use `handle_keys` for decoded host addresses.
    pub async fn keys(&self, context: &Context) -> Result<Vec<String>> {
        let docs = self.check()?;
        let def = lock(&self.slot.state).definition.clone();
        if matches!(def.keyed, KeyedBy::Singleton) {
            return Err(Error::Invalid("keys on singleton".into()));
        }
        if !def.persist {
            let mut keys = lock(&self.slot.state)
                .residents
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            keys.sort();
            return Ok(keys);
        }
        let store = docs
            .0
            .store
            .as_ref()
            .ok_or_else(|| Error::Invalid("persisted document requires a store".into()))?;
        let mut keys = Vec::new();
        let mut after = None;
        loop {
            let lease = docs.0.line.acquire().await?;
            docs.admit(&self.token)?;
            context.check().map_err(|e| Error::Invalid(e.to_string()))?;
            let page = store.keys(def.name.clone(), after).await?;
            lease.release();
            let end = page.len() < 100;
            after = page.last().cloned();
            keys.extend(page);
            if end {
                break;
            }
        }
        Ok(keys)
    }
}
#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Open,
    Committing,
    Stepping,
    Committed,
    Discarded,
    Cut,
}
struct Entry {
    handle: Handle,
    key: String,
    tracker: Option<Tracker>,
    draft: Option<Draft>,
    retire: bool,
    found: Option<Stored>,
}
struct TxState {
    phase: Phase,
    lease: Option<LineLease>,
    entries: IndexMap<(String, String), Entry>,
    pending: usize,
    staged: Vec<Arc<dyn Staged>>,
}
struct TxInner {
    owner: Weak<Inner>,
    token: SealToken,
    context: Context,
    state: Mutex<TxState>,
}
#[derive(Clone)]
pub struct Transaction(Arc<TxInner>);
/// A staged mutation applies inside SQL admission, then adopts only after COMMIT.
pub trait Staged: Send + Sync {
    fn persistent(&self) -> bool {
        false
    }
    fn order(&self) -> crate::store::WriteOrder {
        crate::store::WriteOrder::Write
    }
    fn apply(&self, _step: &mut crate::store::Step<'_>) -> Result<()> {
        Ok(())
    }
    fn adopt(&self) -> Result<()>;
    fn committed(&self) {}
    fn discarded(&self) {}
}
#[derive(Clone)]
pub struct Draft(Arc<Mutex<Change>>);
impl Draft {
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn value(&self) -> Result<Revision> {
        Ok(lock(&self.0).value()?.clone())
    }
    pub fn set(&self, path: &[Segment], value: JsonValue) -> Result<()> {
        Ok(lock(&self.0).set(path, value)?)
    }
    pub fn delete(&self, path: &[Segment]) -> Result<()> {
        Ok(lock(&self.0).delete(path)?)
    }
    pub fn append_text(&self, path: &[Segment], text: JsonString) -> Result<()> {
        Ok(lock(&self.0).append_text(path, text)?)
    }
    pub fn splice(
        &self,
        path: &[Segment],
        index: usize,
        remove: usize,
        items: Vec<JsonValue>,
    ) -> Result<()> {
        Ok(lock(&self.0).splice(path, index, remove, items)?)
    }
    pub fn apply(&self, op: Op) -> Result<()> {
        Ok(lock(&self.0).apply(op)?)
    }
}
struct Pending(Weak<TxInner>);
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(tx) = self.0.upgrade() {
            lock(&tx.state).pending -= 1;
        }
    }
}
struct Adoption {
    slot: Arc<Slot>,
    key: String,
    tracker: Option<Tracker>,
    prepared: Option<Prepared>,
    retire: bool,
    stored: Option<Stored>,
}
impl Transaction {
    fn open(&self) -> Result<Documents> {
        let docs = Documents(
            self.0
                .owner
                .upgrade()
                .ok_or_else(|| released("owner ended"))?,
        );
        if lock(&self.0.state).phase != Phase::Open {
            return Err(released("transaction settled or fenced"));
        }
        docs.admit(&self.0.token)?;
        Ok(docs)
    }
    fn entry(&self, handle: &Handle, key: Option<DocumentKey>) -> Result<(String, String)> {
        let docs = self.open()?;
        handle.check()?;
        handle.same_owner(&docs)?;
        let key = handle.slot.normalize(key)?;
        let name = lock(&handle.slot.state).definition.name.clone();
        let id = (name, key.clone());
        lock(&self.0.state)
            .entries
            .entry(id.clone())
            .or_insert_with(|| Entry {
                handle: handle.clone(),
                key,
                tracker: None,
                draft: None,
                retire: false,
                found: None,
            });
        Ok(id)
    }
    pub fn draft(&self, handle: &Handle, key: Option<DocumentKey>) -> Work<Draft> {
        let id = self.entry(handle, key);
        let tx = self.clone();
        let handle = handle.clone();
        let pending = if id.is_ok() {
            lock(&self.0.state).pending += 1;
            Some(Pending(Arc::downgrade(&self.0)))
        } else {
            None
        };
        Box::pin(async move {
            let _pending = pending;
            let id = id?;
            tx.open()?;
            if let Some(draft) = lock(&tx.0.state)
                .entries
                .get(&id)
                .and_then(|e| e.draft.clone())
            {
                return Ok(draft);
            }
            let retired = lock(&tx.0.state).entries.get(&id).is_some_and(|e| e.retire);
            if !retired {
                handle.slot.load_held(&id.1, &tx.0.token, true).await?;
            }
            tx.open()?;
            let tracker = {
                let s = lock(&handle.slot.state);
                if retired {
                    None
                } else {
                    s.residents.get(&id.1).map(|r| r.tracker.clone())
                }
            };
            let tracker = match tracker {
                Some(tracker) => tracker,
                None => {
                    let s = lock(&handle.slot.state);
                    let initial = if matches!(s.definition.keyed, KeyedBy::Singleton) {
                        s.initial
                            .clone()
                            .ok_or_else(|| Error::Invalid("not_ready".into()))?
                    } else {
                        s.pins
                            .get(&id.1)
                            .and_then(Weak::upgrade)
                            .map(|p| lock(&p.initial).clone())
                            .map(Ok)
                            .unwrap_or_else(|| {
                                Revision::from_json(s.definition.initial()?).map_err(Error::from)
                            })?
                    };
                    Tracker::from_revision(initial)?
                }
            };
            let draft = Draft(Arc::new(Mutex::new(tracker.begin_change())));
            let mut state = lock(&tx.0.state);
            let entry = state
                .entries
                .get_mut(&id)
                .ok_or_else(|| released("transaction settled"))?;
            if let Some(draft) = &entry.draft {
                return Ok(draft.clone());
            }
            entry.tracker = Some(tracker);
            entry.draft = Some(draft.clone());
            Ok(draft)
        })
    }
    pub fn retire(&self, handle: &Handle, key: Option<DocumentKey>) -> Work<()> {
        let id = self.entry(handle, key);
        let tx = self.clone();
        let handle = handle.clone();
        let pending = if id.is_ok() {
            lock(&self.0.state).pending += 1;
            Some(Pending(Arc::downgrade(&self.0)))
        } else {
            None
        };
        Box::pin(async move {
            let _pending = pending;
            let id = id?;
            let docs = tx.open()?;
            let found = {
                let s = lock(&handle.slot.state);
                s.residents.get(&id.1).and_then(|r| r.stored.clone())
            };
            let def = lock(&handle.slot.state).definition.clone();
            let found = if def.persist && found.is_none() {
                let store =
                    docs.0.store.as_ref().ok_or_else(|| {
                        Error::Invalid("persisted document requires a store".into())
                    })?;
                let found = store.read(def.name.clone(), id.1.clone(), false).await?;
                tx.open()?;
                found
                    .map(|found| {
                        def.readable(found.stored.version, &found.fingerprint)?;
                        if matches!(def.keyed, KeyedBy::Host(_)) != found.host.is_some() {
                            return Err(Error::Invalid("stored host key kind differs".into()));
                        }
                        Ok(found.stored)
                    })
                    .transpose()?
            } else {
                found
            };
            let mut s = lock(&tx.0.state);
            let entry = s
                .entries
                .get_mut(&id)
                .ok_or_else(|| released("transaction settled"))?;
            if let Some(draft) = entry.draft.take() {
                lock(&draft.0).abort()
            }
            entry.tracker = None;
            entry.retire = true;
            entry.found = found;
            Ok(())
        })
    }
    pub fn stage(&self, staged: Arc<dyn Staged>) -> Result<()> {
        self.open()?;
        lock(&self.0.state).staged.push(staged);
        Ok(())
    }
    fn cut(&self) {
        let mut s = lock(&self.0.state);
        if !matches!(s.phase, Phase::Open | Phase::Committing) {
            return;
        }
        s.phase = Phase::Cut;
        for entry in s.entries.values() {
            if let Some(draft) = &entry.draft {
                lock(&draft.0).abort()
            }
        }
        if let Some(lease) = s.lease.take() {
            lease.release()
        }
    }
    fn discard(&self) {
        let mut s = lock(&self.0.state);
        if matches!(s.phase, Phase::Committed | Phase::Stepping) {
            return;
        }
        s.phase = Phase::Discarded;
        for entry in s.entries.values() {
            if let Some(draft) = &entry.draft {
                lock(&draft.0).abort()
            }
        }
    }
    async fn commit(&self) -> Result<()> {
        let docs = self.open()?;
        self.0
            .context
            .check()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let mut adoptions = Vec::new();
        let mut plans = Vec::new();
        {
            let mut state = lock(&self.0.state);
            if state.pending != 0 {
                return Err(Error::Invalid(
                    "transaction callback settled with pending operations".into(),
                ));
            }
            state.phase = Phase::Committing;
            for entry in state.entries.values_mut() {
                let slot = &entry.handle.slot;
                let s = lock(&slot.state);
                let existing = s.residents.get(&entry.key);
                let stored = existing
                    .and_then(|r| r.stored.clone())
                    .or_else(|| entry.found.clone());
                let existed = existing.is_some() || entry.found.is_some();
                let prepared = entry
                    .draft
                    .as_ref()
                    .map(|d| lock(&d.0).prepare())
                    .transpose()?;
                let prepared = prepared.filter(|p| {
                    if p.ops.is_empty() {
                        p.abort();
                        false
                    } else {
                        true
                    }
                });
                let retire = entry.retire && existed;
                if prepared.is_none() && !retire {
                    continue;
                }
                let def = s.definition.clone();
                drop(s);
                if def.persist {
                    let mut plan = Plan {
                        key: entry.key.clone(),
                        definition: def,
                        stored,
                        retire,
                        value: prepared.as_ref().map(|p| p.value.clone()),
                        ops: prepared.as_ref().map_or_else(Vec::new, |p| p.ops.clone()),
                        base: false,
                        text: None,
                    };
                    plan.serialize()?;
                    plans.push(plan);
                }
                adoptions.push(Adoption {
                    slot: slot.clone(),
                    key: entry.key.clone(),
                    tracker: entry.tracker.clone(),
                    prepared,
                    retire,
                    stored: None,
                });
            }
        }
        let staged = lock(&self.0.state).staged.clone();
        if plans.is_empty() && !staged.iter().any(|s| s.persistent()) {
            {
                let mut state = lock(&self.0.state);
                if state.phase != Phase::Committing || self.0.token.sealed() {
                    return Err(released("commit fenced"));
                }
                state.phase = Phase::Stepping;
            }
            self.adopt(&docs, adoptions, Vec::new(), Vec::new())?;
            return Ok(());
        }
        let store = docs
            .0
            .store
            .clone()
            .ok_or_else(|| Error::Invalid("persisted document requires a store".into()))?;
        let db = store.database.clone();
        let tx = self.clone();
        let context = tokio::runtime::Handle::current();
        let work = db.call(move |c| {
            {
                let mut s = lock(&tx.0.state);
                if s.phase != Phase::Committing || tx.0.token.sealed() {
                    return Err(released("commit fenced before store admission"));
                }
                s.phase = Phase::Stepping;
            }
            let mut finish = Finish {
                tx: tx.clone(),
                settled: false,
            };
            if let Err(error) = c.execute_batch("BEGIN IMMEDIATE") {
                lock(&tx.0.state).phase = Phase::Discarded;
                return Err(error.into());
            }
            let outcome = store.apply(c, &plans, &staged).and_then(|stored| {
                c.execute_batch("COMMIT")?;
                Ok(stored)
            });
            let result = match outcome {
                Ok((stored, seq, runs)) => {
                    let _runtime = context.enter();
                    let adopted = tx.adopt(&docs, adoptions, stored, runs);
                    store.notify(seq);
                    adopted
                }
                Err(error) => {
                    let rollback = c.execute_batch("ROLLBACK");
                    lock(&tx.0.state).phase = Phase::Discarded;
                    match rollback {
                        Ok(()) => Err(error),
                        Err(rollback) => Err(Error::Rollback {
                            operation: Box::new(error),
                            rollback,
                        }),
                    }
                }
            };
            // The SQL actor owns admitted work even when its caller drops the waiting future.
            finish.settle();
            result
        });
        tokio::pin!(work);
        tokio::select! {biased;result=&mut work=>result,_=self.0.token.signal.cancelled()=>{let phase=lock(&self.0.state).phase;if matches!(phase,Phase::Stepping|Phase::Committed){work.await}else{Err(released("commit fenced"))}}}
    }
    fn adopt(
        &self,
        docs: &Documents,
        mut adoptions: Vec<Adoption>,
        stored: Vec<Option<Stored>>,
        runs: Vec<crate::store::live_query::LiveRun>,
    ) -> Result<()> {
        let mut stored = stored.into_iter();
        for adoption in &mut adoptions {
            if lock(&adoption.slot.state).definition.persist {
                adoption.stored = stored.next().flatten();
            }
        }
        let staged = lock(&self.0.state).staged.clone();
        let result = docs.0.batch.commit(&self.0.context, || {
            for a in adoptions {
                let mut s = lock(&a.slot.state);
                let singleton = matches!(s.definition.keyed, KeyedBy::Singleton);
                let source = if singleton {
                    s.singleton.clone()
                } else {
                    s.pins
                        .get(&a.key)
                        .and_then(Weak::upgrade)
                        .map(|p| p.source.clone())
                };
                let initial = if singleton {
                    s.initial.clone()
                } else {
                    s.pins
                        .get(&a.key)
                        .and_then(Weak::upgrade)
                        .map(|p| lock(&p.initial).clone())
                };
                let before = s
                    .residents
                    .get(&a.key)
                    .map(|r| r.tracker.value())
                    .or_else(|| initial.clone());
                if a.retire {
                    s.residents.shift_remove(&a.key);
                    if let (Some(source), Some(initial)) = (&source, initial.clone()) {
                        source.address_revision(
                            initial.clone(),
                            vec![Op::Replace(initial.to_json())],
                            true,
                            &self.0.context,
                        )?;
                    }
                }
                if let (Some(prepared), Some(tracker)) = (a.prepared, a.tracker) {
                    tracker.adopt(&prepared)?;
                    let version = s.definition.version;
                    s.residents.insert(
                        a.key,
                        Resident {
                            tracker,
                            stored: a.stored,
                            version,
                            touched: Instant::now(),
                        },
                    );
                    if let Some(source) = source {
                        let ops = if !a.retire && before.is_some_and(|b| b.ptr_eq(&prepared.base)) {
                            prepared.ops.clone()
                        } else {
                            vec![Op::Replace(prepared.value.to_json())]
                        };
                        source.address_revision(
                            prepared.value.clone(),
                            ops,
                            false,
                            &self.0.context,
                        )?;
                    }
                }
            }
            for change in &staged {
                change.adopt()?
            }
            crate::store::live_query::adopt(runs)?;
            Ok(())
        });
        // Store admission is irreversible. Always settle the lease even if a projection reports an error.
        lock(&self.0.state).phase = Phase::Committed;
        docs.0.notify.notify_one();
        result
    }
}
struct Finish {
    tx: Transaction,
    settled: bool,
}
impl Finish {
    fn settle(&mut self) {
        if self.settled {
            return;
        }
        if lock(&self.tx.0.state).phase == Phase::Stepping {
            return;
        }
        self.settled = true;
        let (staged, committed, lease) = {
            let mut s = lock(&self.tx.0.state);
            let committed = s.phase == Phase::Committed;
            if !committed {
                s.phase = Phase::Discarded;
                for entry in s.entries.values() {
                    if let Some(draft) = &entry.draft {
                        lock(&draft.0).abort()
                    }
                }
            }
            s.entries.clear();
            (std::mem::take(&mut s.staged), committed, s.lease.take())
        };
        if let Some(owner) = self.tx.0.owner.upgrade() {
            let mut s = lock(&owner.state);
            if s.holder.ptr_eq(&Arc::downgrade(&self.tx.0)) {
                s.holder = Weak::new();
            }
        }
        for change in staged {
            if committed {
                change.committed()
            } else {
                change.discarded()
            }
        }
        // Settled and the next transaction must observe all completion callbacks.
        if let Some(lease) = lease {
            lease.release()
        }
    }
}
impl Drop for Finish {
    fn drop(&mut self) {
        self.settle()
    }
}
