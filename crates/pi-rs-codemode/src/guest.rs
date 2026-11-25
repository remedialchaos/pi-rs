//! Safe Wasmtime embedding of the pinned QuickJS-WASI guest and upstream prelude.
use crate::types::{CancellationToken, HostError, OutputItem};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use wasmtime::{
    AsContextMut, Caller, Config, Engine, Instance, Linker, Memory, Module, Store, WasmParams,
    WasmResults,
};
pub const PRELUDE_SOURCE: &str = include_str!(concat!(env!("OUT_DIR"), "/prelude.js"));
const GUEST: &[u8] = include_bytes!("../assets/quickjs.wasm");
type VmResult<T> = wasmtime::Result<T>;
type Post = Arc<dyn Fn(VmMessage) + Send + Sync>;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmTool {
    pub name: String,
    pub js_name: String,
    pub description: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmGlobal {
    pub name: String,
    pub spread: bool,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Deterministic {
    pub started_at: f64,
    pub seed: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmStartData {
    pub code: String,
    pub tools: Vec<VmTool>,
    pub globals: Vec<VmGlobal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_limit_bytes: Option<usize>,
    pub store: indexmap::IndexMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deterministic: Option<Deterministic>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum VmMessage {
    Failure {
        error: crate::types::CodemodeError,
    },
    Call {
        id: u32,
        target: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        args: Option<String>,
    },
    Output {
        item: OutputItem,
    },
    Done {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        writes: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        during: Option<crate::types::During>,
    },
    Crash {
        message: String,
    },
    Started {
        fingerprint: String,
    },
    Idle {
        seq: u64,
        issued: u64,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmReply {
    pub id: u32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<String>,
}
#[derive(Clone)]
pub struct WasmModule {
    engine: Engine,
    module: Arc<Module>,
}
static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
type ModuleCache = Mutex<HashMap<Option<PathBuf>, Arc<Module>>>;
static MODULES: OnceLock<ModuleCache> = OnceLock::new();
/// Compile once per successful path. Failed reads/compilations are not cached.
pub fn load_quickjs_wasm(path: Option<&Path>) -> Result<WasmModule, HostError> {
    let engine = ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config.max_wasm_stack(2 * 1024 * 1024);
            Engine::new(&config).map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| HostError(e.clone()))?
        .clone();
    let key = path.map(Path::to_path_buf);
    let mut cache = MODULES
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|e| HostError(e.to_string()))?;
    let module = if let Some(module) = cache.get(&key) {
        module.clone()
    } else {
        let bytes = path
            .map(std::fs::read)
            .transpose()
            .map_err(|e| HostError(e.to_string()))?;
        let module = Arc::new(
            Module::new(&engine, bytes.as_deref().unwrap_or(GUEST))
                .map_err(|e| HostError(e.to_string()))?,
        );
        cache.insert(key, module.clone());
        module
    };
    Ok(WasmModule { engine, module })
}
struct Host {
    instance: Option<Instance>,
    memory: Option<Memory>,
    post: Post,
    stop: CancellationToken,
    deadline: Option<Instant>,
    budget: Option<u64>,
    polls: u64,
    deterministic: Option<Deterministic>,
    creating: bool,
    now: f64,
    random: u32,
    origin: Instant,
    seq: u64,
    issued: u64,
}
impl Host {
    fn interrupted(&mut self) -> bool {
        if self.stop.is_cancelled() || self.deadline.is_some_and(|d| Instant::now() >= d) {
            return true;
        }
        self.polls = self.polls.saturating_add(1);
        self.budget.is_some_and(|b| self.polls > b)
    }
}
fn fail(message: impl std::fmt::Display) -> wasmtime::Error {
    wasmtime::Error::msg(message.to_string())
}
fn memory(cx: &mut impl AsContextMut<Data = Host>) -> VmResult<Memory> {
    cx.as_context_mut()
        .data()
        .memory
        .ok_or_else(|| fail("guest memory unavailable"))
}
fn invoke<P: WasmParams, R: WasmResults>(
    cx: &mut impl AsContextMut<Data = Host>,
    name: &str,
    params: P,
) -> VmResult<R> {
    let instance = cx
        .as_context_mut()
        .data()
        .instance
        .ok_or_else(|| fail("guest instance unavailable"))?;
    instance
        .get_typed_func::<P, R>(&mut *cx, name)?
        .call(cx, params)
}
fn read(cx: &mut impl AsContextMut<Data = Host>, ptr: i32, len: usize) -> VmResult<Vec<u8>> {
    let mut bytes = vec![0; len];
    memory(cx)?
        .read(cx, ptr as u32 as usize, &mut bytes)
        .map_err(fail)?;
    Ok(bytes)
}
fn write(cx: &mut impl AsContextMut<Data = Host>, ptr: i32, bytes: &[u8]) -> VmResult<()> {
    memory(cx)?
        .write(cx, ptr as u32 as usize, bytes)
        .map_err(fail)
}
fn u32_at(cx: &mut impl AsContextMut<Data = Host>, ptr: i32) -> VmResult<u32> {
    let b = read(cx, ptr, 4)?;
    Ok(u32::from_le_bytes(
        b.try_into().map_err(|_| fail("invalid word"))?,
    ))
}
fn alloc(cx: &mut impl AsContextMut<Data = Host>, bytes: &[u8]) -> VmResult<i32> {
    let len = i32::try_from(bytes.len()).map_err(fail)?;
    let ptr = invoke::<_, i32>(cx, "wasm_malloc", len)?;
    if ptr == 0 {
        return Err(fail("guest allocation failed"));
    }
    write(cx, ptr, bytes)?;
    Ok(ptr)
}
fn free(cx: &mut impl AsContextMut<Data = Host>, ptr: i32) -> VmResult<()> {
    invoke(cx, "wasm_free", ptr)
}
fn release(cx: &mut impl AsContextMut<Data = Host>, handle: i32) -> VmResult<()> {
    invoke(cx, "qjs_free_value", handle)
}
fn new_string(cx: &mut impl AsContextMut<Data = Host>, text: &str) -> VmResult<i32> {
    let ptr = alloc(cx, text.as_bytes())?;
    let value = invoke(cx, "qjs_new_string", (ptr, text.len() as i32));
    free(cx, ptr)?;
    value
}
fn string(cx: &mut impl AsContextMut<Data = Host>, handle: i32) -> VmResult<String> {
    let len_ptr = alloc(cx, &[0; 4])?;
    let ptr = invoke::<_, i32>(cx, "qjs_get_string_len", (handle, len_ptr))?;
    let len = u32_at(cx, len_ptr)? as usize;
    free(cx, len_ptr)?;
    if ptr == 0 {
        return Err(fail("value is not convertible to a string"));
    }
    let bytes = read(cx, ptr, len)?;
    invoke::<_, ()>(cx, "qjs_free_cstring", ptr)?;
    String::from_utf8(bytes).map_err(fail)
}
fn optional_string(
    cx: &mut impl AsContextMut<Data = Host>,
    handle: i32,
) -> VmResult<Option<String>> {
    if handle == 0 || invoke::<_, i32>(cx, "qjs_is_undefined", handle)? != 0 {
        Ok(None)
    } else {
        string(cx, handle).map(Some)
    }
}
fn property(cx: &mut impl AsContextMut<Data = Host>, handle: i32, name: &str) -> VmResult<i32> {
    let ptr = alloc(cx, format!("{name}\0").as_bytes())?;
    let value = invoke(cx, "qjs_get_prop_string", (handle, ptr));
    free(cx, ptr)?;
    value
}
fn checked(cx: &mut impl AsContextMut<Data = Host>, handle: i32) -> VmResult<i32> {
    if invoke::<_, i32>(cx, "qjs_is_exception", handle)? == 0 {
        return Ok(handle);
    }
    release(cx, handle)?;
    let exception = invoke::<_, i32>(cx, "qjs_get_exception", ())?;
    let mut fields = serde_json::Map::new();
    for name in ["name", "message", "stack"] {
        let value = property(cx, exception, name)?;
        if let Some(text) = optional_string(cx, value)? {
            fields.insert(name.into(), text.into());
        }
        release(cx, value)?;
    }
    release(cx, exception)?;
    let name = fields
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Error");
    let message = fields.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let head = if message.is_empty() {
        name.to_owned()
    } else {
        format!("{name}: {message}")
    };
    let stack = fields
        .get("stack")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_end();
    fields.insert(
        "stack".into(),
        if stack.is_empty() {
            head
        } else {
            format!("{head}\n{stack}")
        }
        .into(),
    );
    Err(fail(serde_json::Value::Object(fields)))
}
fn eval(cx: &mut impl AsContextMut<Data = Host>, code: &str, filename: &str) -> VmResult<i32> {
    let code_ptr = alloc(cx, format!("{code}\0").as_bytes())?;
    let file_ptr = alloc(cx, format!("{filename}\0").as_bytes())?;
    let result = invoke::<_, i32>(cx, "qjs_eval", (code_ptr, code.len() as i32, file_ptr, 0));
    free(cx, code_ptr)?;
    free(cx, file_ptr)?;
    checked(cx, result?)
}
fn call(
    cx: &mut impl AsContextMut<Data = Host>,
    function: i32,
    this: i32,
    args: &[i32],
) -> VmResult<i32> {
    let bytes: Vec<_> = args.iter().flat_map(|a| a.to_le_bytes()).collect();
    let ptr = if bytes.is_empty() {
        0
    } else {
        alloc(cx, &bytes)?
    };
    let result = invoke::<_, i32>(cx, "qjs_call", (function, this, args.len() as i32, ptr));
    if ptr != 0 {
        free(cx, ptr)?;
    }
    checked(cx, result?)
}
fn bridge(
    mut cx: Caller<'_, Host>,
    _name: i32,
    _len: i32,
    _this: i32,
    argc: i32,
    argv: i32,
) -> VmResult<i32> {
    let mut args = [0; 4];
    for (index, arg) in args.iter_mut().enumerate().take(argc.clamp(0, 4) as usize) {
        *arg = u32_at(&mut cx, argv + index as i32 * 4)? as i32;
    }
    let kind = string(&mut cx, args[0])?;
    let message = match kind.as_str() {
        "call" | "global" => {
            cx.data_mut().issued += 1;
            VmMessage::Call {
                id: invoke::<_, f64>(&mut cx, "qjs_get_float64", args[1])? as u32,
                target: if kind == "call" { "tool" } else { "global" }.into(),
                name: string(&mut cx, args[2])?,
                args: optional_string(&mut cx, args[3])?,
            }
        }
        "output" => {
            let kind = string(&mut cx, args[1])?;
            let text = string(&mut cx, args[2])?;
            VmMessage::Output {
                item: if kind == "image" {
                    OutputItem::Image {
                        data: text,
                        mime_type: string(&mut cx, args[3])?,
                    }
                } else {
                    OutputItem::Text {
                        text,
                        console: kind == "console",
                    }
                },
            }
        }
        "done" => {
            let ok = invoke::<_, i32>(&mut cx, "qjs_get_bool", args[1])? != 0;
            let payload = optional_string(&mut cx, args[2])?;
            let during = cx.data().deterministic.map(|_| crate::types::During {
                seq: cx.data().seq,
                issued: cx.data().issued,
            });
            VmMessage::Done {
                ok,
                value: if ok { payload.clone() } else { None },
                writes: if ok {
                    optional_string(&mut cx, args[3])?
                } else {
                    None
                },
                error: if ok { None } else { payload },
                during,
            }
        }
        _ => return invoke(&mut cx, "qjs_get_undefined", ()),
    };
    let message = if matches!(message, VmMessage::Done { .. } | VmMessage::Crash { .. }) {
        if cx
            .data()
            .budget
            .is_some_and(|budget| cx.data().polls > budget)
        {
            VmMessage::Failure {
                error: crate::types::CodemodeError::new(
                    crate::types::ErrorKind::Timeout,
                    format!(
                        "Execution exceeded its interrupt budget of {}",
                        cx.data().budget.unwrap_or(0)
                    ),
                ),
            }
        } else if cx
            .data()
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            VmMessage::Failure {
                error: crate::types::CodemodeError::new(
                    crate::types::ErrorKind::Timeout,
                    "Execution timed out",
                ),
            }
        } else {
            message
        }
    } else {
        message
    };
    (cx.data().post)(message);
    invoke(&mut cx, "qjs_get_undefined", ())
}
fn linker(engine: &Engine) -> VmResult<Linker<Host>> {
    let mut linker = Linker::new(engine);
    linker.func_wrap("env", "host_call", bridge)?;
    linker.func_wrap("env", "host_interrupt", |mut cx: Caller<'_, Host>| -> i32 {
        i32::from(cx.data_mut().interrupted())
    })?;
    linker.func_wrap(
        "env",
        "host_promise_rejection",
        |mut cx: Caller<'_, Host>, promise: i32, reason: i32, _handled: i32| -> VmResult<()> {
            release(&mut cx, promise)?;
            release(&mut cx, reason)
        },
    )?;
    linker.func_wrap(
        "env",
        "host_module_normalize",
        |_cx: Caller<'_, Host>, _base: i32, _name: i32| -> i32 { 0 },
    )?;
    linker.func_wrap(
        "env",
        "host_module_load",
        |_cx: Caller<'_, Host>, _name: i32, _out: i32| -> i32 { 0 },
    )?;
    linker.func_wrap(
        "env",
        "host_get_timezone_offset",
        |cx: Caller<'_, Host>, hi: i32, lo: i32| -> i32 {
            if cx.data().deterministic.is_some() {
                return 0;
            }
            let seconds = (i64::from(hi) << 32) | i64::from(lo as u32);
            let Ok(timestamp) = jiff::Timestamp::from_second(seconds) else {
                return 0;
            };
            let zone = std::env::var("TZ")
                .ok()
                .and_then(|name| jiff::tz::TimeZone::get(&name).ok())
                .unwrap_or_else(jiff::tz::TimeZone::system);
            zone.to_offset(timestamp).seconds()
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "clock_time_get",
        |mut cx: Caller<'_, Host>, clock: i32, _precision: i64, ptr: i32| -> VmResult<i32> {
            let data = cx.data();
            let nanos = if let Some(d) = data.deterministic {
                if data.creating {
                    if clock == 0 {
                        (u64::from(d.seed) + 1) * 1000
                    } else {
                        0
                    }
                } else {
                    let ms = if clock == 0 {
                        data.now
                    } else {
                        data.now - d.started_at
                    };
                    (ms.round() as u64).saturating_mul(1_000_000)
                }
            } else if clock == 0 {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(fail)?
                    .as_nanos() as u64
            } else {
                data.origin.elapsed().as_nanos() as u64
            };
            write(&mut cx, ptr, &nanos.to_le_bytes())?;
            Ok(0)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "random_get",
        |mut cx: Caller<'_, Host>, ptr: i32, len: i32| -> VmResult<i32> {
            let mut bytes = vec![0; len as u32 as usize];
            if cx.data().deterministic.is_some() {
                for chunk in bytes.chunks_mut(4) {
                    let state = cx.data().random.wrapping_add(0x6d2b79f5);
                    cx.data_mut().random = state;
                    let mut value = (state ^ (state >> 15)).wrapping_mul(1 | state);
                    value =
                        value.wrapping_add((value ^ (value >> 7)).wrapping_mul(61 | value)) ^ value;
                    let value = (value ^ (value >> 14)).to_le_bytes();
                    chunk.copy_from_slice(&value[..chunk.len()]);
                }
            } else {
                getrandom::fill(&mut bytes).map_err(fail)?;
            }
            write(&mut cx, ptr, &bytes)?;
            Ok(0)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_write",
        |mut cx: Caller<'_, Host>, _fd: i32, iovs: i32, len: i32, out: i32| -> VmResult<i32> {
            let mut total = 0u32;
            for index in 0..len {
                total = total.wrapping_add(u32_at(&mut cx, iovs + index * 8 + 4)?);
            }
            write(&mut cx, out, &total.to_le_bytes())?;
            Ok(0)
        },
    )?;
    linker.func_wrap("wasi_snapshot_preview1", "fd_close", |_fd: i32| -> i32 {
        52
    })?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_seek",
        |_fd: i32, _offset: i64, _whence: i32, _out: i32| -> i32 { 52 },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_fdstat_get",
        |mut cx: Caller<'_, Host>, fd: i32, ptr: i32| -> VmResult<i32> {
            if fd != 1 && fd != 2 {
                return Ok(8);
            }
            let mut stat = [0; 24];
            stat[0] = 2;
            write(&mut cx, ptr, &stat)?;
            Ok(0)
        },
    )?;
    Ok(linker)
}
pub struct VmSession {
    store: Store<Host>,
    api: i32,
    settle: i32,
    stalled: i32,
    next_seq: u64,
    early: BTreeMap<u64, VmReply>,
}
impl VmSession {
    pub fn start(
        data: VmStartData,
        module: WasmModule,
        post: Post,
        stop: CancellationToken,
        deadline: Option<Instant>,
        budget: Option<u64>,
        stack: usize,
    ) -> Result<Self, HostError> {
        Self::create(data, module, post, stop, deadline, budget, stack)
            .map_err(|e| HostError(e.to_string()))
    }
    fn create(
        data: VmStartData,
        module: WasmModule,
        post: Post,
        stop: CancellationToken,
        deadline: Option<Instant>,
        budget: Option<u64>,
        stack: usize,
    ) -> VmResult<Self> {
        let mut store = Store::new(
            &module.engine,
            Host {
                instance: None,
                memory: None,
                post,
                stop,
                deadline,
                budget,
                polls: 0,
                deterministic: data.deterministic,
                creating: true,
                now: data.deterministic.map_or(0.0, |d| d.started_at),
                random: data.deterministic.map_or(0, |d| d.seed ^ 0x9e3779b9),
                origin: Instant::now(),
                seq: 0,
                issued: 0,
            },
        );
        let instance = linker(&module.engine)?.instantiate(&mut store, &module.module)?;
        store.data_mut().instance = Some(instance);
        store.data_mut().memory = instance.get_memory(&mut store, "memory");
        invoke::<_, ()>(&mut store, "_initialize", ())?;
        let intrinsics = if data.deterministic.is_some() {
            !512u32
        } else {
            u32::MAX
        };
        let status = invoke::<_, i32>(&mut store, "qjs_init2", intrinsics as i32)?;
        if status != 0 {
            return Err(fail("Failed to initialize QuickJS"));
        }
        if let Some(limit) = data.memory_limit_bytes {
            invoke::<_, ()>(
                &mut store,
                "qjs_set_memory_limit",
                i32::try_from(limit).map_err(fail)?,
            )?;
        }
        invoke::<_, ()>(&mut store, "qjs_set_max_stack_size", stack as i32)?;
        invoke::<_, ()>(&mut store, "qjs_set_interrupt_handler", 1i32)?;
        store.data_mut().creating = false;
        let undefined = invoke::<_, i32>(&mut store, "qjs_get_undefined", ())?;
        let name = alloc(&mut store, b"bridge\0")?;
        let bridge = invoke::<_, i32>(&mut store, "qjs_new_host_function", (name, 6i32, 0i32))?;
        free(&mut store, name)?;
        let prelude = eval(&mut store, PRELUDE_SOURCE, "codemode-prelude.js")?;
        let tools = new_string(
            &mut store,
            &serde_json::to_string(&data.tools).map_err(fail)?,
        )?;
        let globals = new_string(
            &mut store,
            &serde_json::to_string(&data.globals).map_err(fail)?,
        )?;
        let snapshot = new_string(
            &mut store,
            &serde_json::to_string(&data.store).map_err(fail)?,
        )?;
        let api = call(
            &mut store,
            prelude,
            undefined,
            &[bridge, tools, globals, snapshot],
        )?;
        for handle in [undefined, bridge, prelude, tools, globals, snapshot] {
            release(&mut store, handle)?;
        }
        let settle = property(&mut store, api, "settle")?;
        let stalled = property(&mut store, api, "stalled")?;
        let run = property(&mut store, api, "run")?;
        let mut session = Self {
            store,
            api,
            settle,
            stalled,
            next_seq: 1,
            early: BTreeMap::new(),
        };
        if data.deterministic.is_some() {
            let ptr = invoke::<_, i32>(&mut session.store, "qjs_get_quickjs_version", ())?;
            let mem = memory(&mut session.store)?;
            let bytes = mem
                .data(&session.store)
                .get(ptr as u32 as usize..)
                .ok_or_else(|| fail("invalid version pointer"))?;
            let end = bytes
                .iter()
                .position(|b| *b == 0)
                .ok_or_else(|| fail("unterminated version"))?;
            let version = std::str::from_utf8(&bytes[..end]).map_err(fail)?;
            let hash = PRELUDE_SOURCE.encode_utf16().fold(0x811c9dc5u32, |h, c| {
                (h ^ u32::from(c)).wrapping_mul(0x01000193)
            });
            let fingerprint = format!(
                "quickjs-wasi 3.6.2, quickjs {version}, prelude {hash:08x}, deterministic 1, stack {stack}, heap {}, budget {}",
                data.memory_limit_bytes
                    .map_or("none".into(), |n| n.to_string()),
                budget.map_or("none".into(), |n| n.to_string())
            );
            (session.store.data().post)(VmMessage::Started { fingerprint });
        }
        match eval(
            &mut session.store,
            &format!("(async (tools, console) => {{{}\n}})", data.code),
            "codemode.js",
        ) {
            Ok(function) => {
                let result = call(&mut session.store, run, api, &[function])?;
                release(&mut session.store, result)?;
                release(&mut session.store, function)?;
                release(&mut session.store, run)?;
                session.drain()?;
                session.idle(0);
            }
            Err(error) => (session.store.data().post)(VmMessage::Done {
                ok: false,
                value: None,
                writes: None,
                error: Some(error.to_string()),
                during: None,
            }),
        }
        Ok(session)
    }
    fn idle(&self, seq: u64) {
        if self.store.data().deterministic.is_some() {
            (self.store.data().post)(VmMessage::Idle {
                seq,
                issued: self.store.data().issued,
            });
        }
    }
    fn drain(&mut self) -> VmResult<()> {
        let mut jobs = 0;
        while invoke::<_, i32>(&mut self.store, "qjs_is_job_pending", ())? != 0 {
            if invoke::<_, i32>(&mut self.store, "qjs_execute_pending_job", ())? < 0 {
                return Err(fail("Promise job failed"));
            }
            jobs += 1;
            if jobs % 50 == 0 && self.store.data_mut().interrupted() {
                return Err(fail("Execution interrupted"));
            }
        }
        let result = call(&mut self.store, self.stalled, self.api, &[])?;
        release(&mut self.store, result)
    }
    fn apply(&mut self, reply: VmReply) -> VmResult<()> {
        let id = invoke::<_, i32>(&mut self.store, "qjs_new_number", f64::from(reply.id))?;
        let ok = invoke::<_, i32>(
            &mut self.store,
            if reply.ok {
                "qjs_get_true"
            } else {
                "qjs_get_false"
            },
            (),
        )?;
        let payload = if let Some(value) = reply.payload {
            new_string(&mut self.store, &value)?
        } else {
            invoke(&mut self.store, "qjs_get_undefined", ())?
        };
        let properties = if let Some(value) = reply.properties {
            new_string(&mut self.store, &value)?
        } else {
            invoke(&mut self.store, "qjs_get_undefined", ())?
        };
        let result = call(
            &mut self.store,
            self.settle,
            self.api,
            &[id, ok, payload, properties],
        )?;
        for handle in [id, ok, payload, properties, result] {
            release(&mut self.store, handle)?;
        }
        self.drain()
    }
    pub fn deliver(&mut self, reply: VmReply) -> Result<(), HostError> {
        self.deliver_inner(reply)
            .map_err(|e| HostError(e.to_string()))
    }
    fn deliver_inner(&mut self, reply: VmReply) -> VmResult<()> {
        if self.store.data().deterministic.is_none() {
            return self.apply(reply);
        }
        let seq = reply
            .seq
            .ok_or_else(|| fail("Result is missing sequence number"))?;
        if seq < self.next_seq || self.early.contains_key(&seq) {
            return Err(fail(format!(
                "Result {} has sequence number {seq}, expected {} or later, once",
                reply.id, self.next_seq
            )));
        }
        self.early.insert(seq, reply);
        while let Some(reply) = self.early.remove(&self.next_seq) {
            if let Some(now) = reply.now.filter(|n| n.is_finite()) {
                self.store.data_mut().now = self.store.data().now.max(now);
            }
            self.store.data_mut().seq = self.next_seq;
            self.apply(reply)?;
            self.idle(self.next_seq);
            self.next_seq += 1;
        }
        Ok(())
    }
    pub(crate) fn interrupt_budget(&self) -> Option<u64> {
        self.store.data().budget
    }
    pub fn budget_exhausted(&self) -> bool {
        self.store
            .data()
            .budget
            .is_some_and(|b| self.store.data().polls > b)
    }
}
