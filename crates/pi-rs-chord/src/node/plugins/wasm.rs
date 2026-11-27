use crate::{
    Error, Result,
    context::Context,
    contracts::{Contract, Member},
    host::{EventChannel, Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
    state::{Batch, Source},
};
use indexmap::IndexMap;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use wasmtime::{
    Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, HasSelf, Linker},
};

pub mod bindings {
    wasmtime::component::bindgen!({path:"wit/plugin.wit",world:"plugin",imports:{default:async},exports:{default:async}});
}
#[derive(Clone)]
enum Publisher {
    State(Source),
    Document(crate::host::ObjectDocument),
    Event(EventChannel),
    Keyed(Keyed),
}
#[derive(Clone)]
struct Keyed {
    batch: Batch,
    initial: JsonValue,
    entries: Arc<Mutex<BTreeMap<String, Source>>>,
}
impl Keyed {
    fn source(&self, key: &JsonValue) -> Result<Source> {
        let JsonValue::String(key) = key else {
            return Err(failure("keyed member requires a string key"));
        };
        let key = key.to_json();
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(source) = entries.get(&key) {
            return Ok(source.clone());
        }
        let source = Source::new(self.batch.clone(), self.initial.clone())?;
        entries.insert(key, source.clone());
        Ok(source)
    }
}
type Sources = Arc<Mutex<BTreeMap<(String, String), Publisher>>>;
type Logger = Arc<dyn Fn(&str) + Send + Sync>;
struct GuestState {
    limits: StoreLimits,
    sources: Sources,
    active_object: Option<String>,
    logger: Logger,
}
impl bindings::chord::plugin::host::Host for GuestState {
    async fn log(&mut self, message: String) {
        (self.logger)(&message);
    }
    async fn read(
        &mut self,
        object: String,
        member: String,
        key_json: Option<String>,
    ) -> std::result::Result<String, String> {
        if self.active_object.as_deref() != Some(&object) {
            return Err("read requires the current object".into());
        }
        let publisher = self
            .sources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(object, member))
            .cloned()
            .ok_or("unknown readable")?;
        let value = match publisher {
            Publisher::Document(doc) => doc.value(),
            Publisher::State(source) => source.value().map(|revision| revision.to_json()),
            Publisher::Keyed(keyed) => {
                let key = JsonValue::parse(key_json.as_deref().ok_or("missing key")?)
                    .map_err(|e| e.to_string())?;
                keyed
                    .source(&key)
                    .and_then(|source| source.value())
                    .map(|revision| revision.to_json())
            }
            Publisher::Event(_) => return Err("events have no current value".into()),
        };
        value
            .and_then(|value| value.to_json())
            .map_err(|e| e.to_string())
    }
    async fn publish(
        &mut self,
        object: String,
        member: String,
        value_json: String,
    ) -> std::result::Result<(), String> {
        if self.active_object.as_deref() != Some(&object) {
            return Err("publish requires the current object".into());
        }
        let value = JsonValue::parse(&value_json).map_err(|e| e.to_string())?;
        let source = self
            .sources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(object, member))
            .cloned()
            .ok_or_else(|| "unknown readable".to_string())?;
        match source {
            Publisher::Document(document) => {
                document
                    .change(&Context::background(), move |draft| {
                        Box::pin(async move { draft.apply(crate::delta::Op::Replace(value)) })
                    })
                    .await
            }
            Publisher::State(source) => source.set(value, &Context::background()),
            Publisher::Event(event) => {
                event.emit(value);
                Ok(())
            }
            Publisher::Keyed(keyed) => {
                let key = value.get("key").ok_or("missing key")?;
                let value = value.get("value").ok_or("missing value")?;
                keyed
                    .source(key)
                    .and_then(|source| source.set(value.clone(), &Context::background()))
            }
        }
        .map_err(|e| e.to_string())
    }
}
struct Guest {
    store: Store<GuestState>,
    instance: bindings::Plugin,
}
async fn invoke(
    guest: &Arc<tokio::sync::Mutex<Guest>>,
    object: &str,
    name: &str,
    args: JsonValue,
    principal: &(impl serde::Serialize + Sync),
) -> Result<JsonValue> {
    let mut guest = guest.lock().await;
    guest.call(object, name, args, principal, true).await
}
impl Guest {
    async fn call(
        &mut self,
        object: &str,
        name: &str,
        args: JsonValue,
        principal: &(impl serde::Serialize + Sync),
        access_object: bool,
    ) -> Result<JsonValue> {
        let args = args.to_json()?;
        let principal = serde_json::to_string(principal)?;
        self.store.set_fuel(10_000_000).map_err(failure)?;
        self.store.data_mut().active_object = access_object.then(|| object.to_owned());
        let result = self
            .instance
            .call_invoke(&mut self.store, object, name, &args, &principal)
            .await;
        self.store.data_mut().active_object = None;
        JsonValue::parse(&result.map_err(failure)?.map_err(failure)?)
    }
}
#[derive(Clone)]
pub struct WasmPluginHost {
    engine: Engine,
    logger: Logger,
}
fn failure(error: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("WASM plugin: {error}"))
}
impl WasmPluginHost {
    pub fn new() -> Result<Self> {
        Self::with_logger(|message| {
            use std::io::Write;
            let _ = writeln!(std::io::stderr().lock(), "{message}");
        })
    }
    pub fn with_logger(logger: impl Fn(&str) + Send + Sync + 'static) -> Result<Self> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true).consume_fuel(true);
        Ok(Self {
            logger: Arc::new(logger),
            engine: Engine::new(&config).map_err(failure)?,
        })
    }
    fn store(&self, sources: Sources) -> Result<Store<GuestState>> {
        let mut store = Store::new(
            &self.engine,
            GuestState {
                limits: StoreLimitsBuilder::new()
                    .memory_size(64 * 1024 * 1024)
                    .table_elements(100_000)
                    .instances(100)
                    .memories(10)
                    .build(),
                sources,
                active_object: None,
                logger: self.logger.clone(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_fuel(10_000_000).map_err(failure)?;
        store
            .fuel_async_yield_interval(Some(10_000))
            .map_err(failure)?;
        Ok(store)
    }
    /// Validate and instantiate a component, without requiring the plugin world's exports.
    pub async fn instantiate_component(&self, bytes: &[u8]) -> Result<()> {
        let component = Component::new(&self.engine, bytes).map_err(failure)?;
        let linker = Linker::<GuestState>::new(&self.engine);
        let mut store = self.store(Arc::default())?;
        linker
            .instantiate_async(&mut store, &component)
            .await
            .map_err(failure)?;
        Ok(())
    }
    pub async fn prepare(&self, bytes: &[u8]) -> Result<Facet> {
        let sources = Sources::default();
        let component = Component::new(&self.engine, bytes).map_err(failure)?;
        let mut linker = Linker::new(&self.engine);
        bindings::Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s).map_err(failure)?;
        let mut store = self.store(sources.clone())?;
        let instance = bindings::Plugin::instantiate_async(&mut store, &component, &linker)
            .await
            .map_err(failure)?;
        let mut guest = Guest { store, instance };
        let declarations = guest
            .instance
            .call_setup(&mut guest.store)
            .await
            .map_err(failure)?
            .map_err(failure)?;
        guest.facet(&declarations, sources)
    }
}
mod manifest;
#[cfg(test)]
mod tests;
