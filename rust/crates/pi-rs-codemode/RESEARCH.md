# ADR-010 host ABI research

Primary sources inspected in full: installed quickjs-wasi 3.6.2 dist/index.js and dist/wasi-shim.js, upstream packages/codemode/src/runtime/{vm,prelude-source,execution,inline,remote,protocol}.ts, src/types.ts and src/wasm.ts. Wasmtime embedding API reference: https://docs.rs/wasmtime/50.0.2/wasmtime/ and the Config, Store, Linker, Caller, Memory and TypedFunc pages.

Use Wasmtime 50.0.2 with std, runtime and cranelift, without WASI or native QuickJS dependencies. Compile modules with one shared Engine and cache successful Modules by path. Instantiate a fresh Store per execution. All pointers are guest u32 offsets and all reads and writes use checked Memory APIs. Handles returned by qjs_* are owned and must be freed. host_call argv handles are borrowed. Host callbacks return a fresh owned undefined handle. Guest export calls are reentrant through Caller.

Imports: env.host_call(namePtr,nameLen,thisPtr,argc,argvPtr), host_interrupt(), host_promise_rejection(promise,reason,handled), host_module_normalize(base,name), host_module_load(name,outLen), host_get_timezone_offset(hi,lo). WASI preview1 imports are clock_time_get, random_get, fd_write, fd_close, fd_fdstat_get and fd_seek. No filesystem or networking imports are supplied. Writes are discarded exactly as upstream.

Initialization: _initialize, qjs_init2(intrinsics), then qjs_set_memory_limit, qjs_set_max_stack_size and qjs_set_interrupt_handler. Evaluation uses UTF-8 source and NUL terminated filename. qjs_eval/qjs_call return a boxed handle, checked by qjs_is_exception. Strings use qjs_get_string_len and qjs_free_cstring. Pending jobs use qjs_is_job_pending/qjs_execute_pending_job, with an interrupt check after 50 jobs. The guest contains QuickJS and its stack overflow remains a catchable RangeError.

The upstream prelude snapshot is vendored unchanged. A Rust build script decodes its TypeScript template literal and substitutes only its declared constants. No replacement JavaScript is authored. The resulting exact PRELUDE_SOURCE owns lockdown, guarded tool/global proxies, JSON transport, output/store limits, image validation, settlement and error formatting.

Deterministic mode: omit intrinsic bit 9, use UTC, mulberry32 seed xor 0x9e3779b9, seeded initial real clock then a monotonic delivery clock. The fingerprint includes guest version, FNV-1a over prelude UTF-16, stack, heap and interrupt budget. Result deliveries are applied in consecutive seq order starting at 1.

Jiff 1.0.2 supplies portable host timezone offsets: Timestamp::from_second, TimeZone::get/system and TimeZone::to_offset().seconds(). The QuickJS import takes split signed seconds, returns offset seconds, and the guest converts it to Date.getTimezoneOffset minutes. Verified on a child process with TZ=Asia/Kolkata: non-deterministic offset -330, deterministic offset 0. Reading TZ does not mutate process environment.

All authored host and build code is Rust and forbids unsafe. No proof crate or upstream source is edited.
