use crate::{
    Error, Result,
    context::Context,
    contracts::{Contract, Member},
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
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
type Sources = Arc<Mutex<BTreeMap<(String, String), Source>>>;
struct GuestState {
    limits: StoreLimits,
    sources: Sources,
    staged: bool,
}
impl bindings::chord::plugin::host::Host for GuestState {
    async fn log(&mut self, _message: String) {}
    async fn publish(
        &mut self,
        object: String,
        member: String,
        value_json: String,
    ) -> std::result::Result<(), String> {
        if self.staged {
            return Err("publish is forbidden during setup".into());
        }
        let value = serde_json::from_str(&value_json).map_err(|e| e.to_string())?;
        let source = self
            .sources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(object, member))
            .cloned()
            .ok_or_else(|| "unknown readable".to_string())?;
        source
            .set(JsonValue::from_serde(value), &Context::background())
            .map_err(|e| e.to_string())
    }
}
struct Guest {
    store: Store<GuestState>,
    instance: bindings::Plugin,
}
#[derive(Clone)]
pub struct WasmPluginHost {
    engine: Engine,
}
fn failure(error: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("WASM plugin: {error}"))
}
impl WasmPluginHost {
    pub fn new() -> Result<Self> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true).consume_fuel(true);
        Ok(Self {
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
                staged: true,
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
        let component = Component::new(&self.engine, bytes).map_err(failure)?;
        let sources = Sources::default();
        let mut linker = Linker::new(&self.engine);
        bindings::Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s).map_err(failure)?;
        let mut store = self.store(sources.clone())?;
        let instance = bindings::Plugin::instantiate_async(&mut store, &component, &linker)
            .await
            .map_err(failure)?;
        let declarations = instance
            .call_setup(&mut store)
            .await
            .map_err(failure)?
            .map_err(failure)?;
        let declarations: Vec<TypeDeclaration> = serde_json::from_str(&declarations)?;
        // Validate the complete declaration set before any facet can be staged.
        let mut types = Vec::new();
        for declaration in declarations {
            let mut members = IndexMap::new();
            for (name, member) in &declaration.members {
                let schema = JsonValue::from_serde(member.schema.clone());
                let kind = match member.kind.as_str() {
                    "state" => Member::State(schema),
                    "method" => crate::contracts::schema::method(
                        member.args.clone().map(JsonValue::from_serde),
                        member.result.clone().map(JsonValue::from_serde),
                    ),
                    _ => return Err(failure("unknown member kind")),
                };
                members.insert(name.clone(), kind);
            }
            let contract = Arc::new(Contract::local(
                &declaration.name,
                u64::from(declaration.version),
                vec![],
                members,
            )?);
            types.push((contract, declaration));
        }
        store.data_mut().staged = false;
        let guest = Arc::new(tokio::sync::Mutex::new(Guest { store, instance }));
        Ok(define_facet(move |setup| {
            for (contract, declaration) in &types {
                let sources = sources.clone();
                let guest = guest.clone();
                let declaration = declaration.clone();
                setup.serve(contract.clone(),move |params,principal,self_| {
                    let object=serde_json::to_string(&serde_json::json!({"type":declaration.name,"params":params.to_serde()?,"principal":principal}))?;
                    let mut members=Members::new();
                    for (name,member) in &declaration.members {
                        if member.kind=="state" {
                            let source=self_.writable(JsonValue::from_serde(member.initial.clone().unwrap_or(serde_json::Value::Null)))?;
                            let key=(object.clone(),name.clone());
                            let opened=sources.clone();
                            let closed=sources.clone();
                            let readable=source.clone();
                            self_.open(move |_| Box::pin(async move {
                                opened.lock().unwrap_or_else(|e|e.into_inner()).insert(key.clone(),readable);
                                Ok(key)
                            }),move |key,_| Box::pin(async move {
                                closed.lock().unwrap_or_else(|e|e.into_inner()).remove(key.as_ref());
                                Ok(())
                            }));
                            members.state(name,source)?;
                        } else {
                            let (guest,object,name)=(guest.clone(),object.clone(),name.clone());
                            members.method(name.clone(),move |args,ctx| {
                                let (guest,object,name)=(guest.clone(),object.clone(),name.clone());
                                Box::pin(async move {
                                    let args=args.to_json()?;
                                    let principal=serde_json::to_string(&ctx.principal)?;
                                    let mut guest=guest.lock().await;
                                    let Guest {store,instance}=&mut *guest;
                                    store.set_fuel(10_000_000).map_err(failure)?;
                                    let value=instance.call_invoke(&mut *store,&object,&name,&args,&principal).await.map_err(failure)?.map_err(failure)?;
                                    Ok(JsonValue::from_serde(serde_json::from_str(&value)?))
                                })
                            })?;
                        }
                    }
                    Ok(members)
                },ServeOptions::default())?;
            }
            Ok(())
        }))
    }
}
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeDeclaration {
    name: String,
    version: u32,
    members: IndexMap<String, MemberDeclaration>,
}
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberDeclaration {
    kind: String,
    #[serde(default)]
    schema: serde_json::Value,
    initial: Option<serde_json::Value>,
    args: Option<serde_json::Value>,
    result: Option<serde_json::Value>,
}
