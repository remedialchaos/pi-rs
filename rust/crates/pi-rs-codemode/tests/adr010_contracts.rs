use futures::{FutureExt, StreamExt, channel::mpsc};
use pi_rs_codemode::*;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type TestResult = Result<(), Box<dyn std::error::Error>>;
#[derive(Clone, Copy)]
enum Backend {
    Worker,
    Inline,
    Remote,
}
fn remote_factory(
    max_budget: u64,
    heap: Option<usize>,
    batches: Arc<Mutex<Vec<usize>>>,
    reorder: bool,
) -> Result<RemoteFactory, HostError> {
    let wasm = load_quickjs_wasm(None)?;
    let executor: RemoteExecutor = Arc::new(move |request, exchange| {
        let options = ServeOptions {
            wasm: wasm.clone(),
            max_interrupt_budget: Some(max_budget),
            max_memory_limit_bytes: heap,
        };
        let batches = batches.clone();
        async move {
            let request = serde_json::from_value(serde_json::to_value(request).map_err(|e| HostError(e.to_string()))?).map_err(|e| HostError(e.to_string()))?;
            let wrapped: RemoteExchange = Arc::new(move |messages| {
                let exchange = exchange.clone(); let batches = batches.clone();
                async move {
                    batches.lock().map_err(|e| HostError(e.to_string()))?.push(messages.len());
                    let messages = serde_json::from_value(serde_json::to_value(messages).map_err(|e| HostError(e.to_string()))?).map_err(|e| HostError(e.to_string()))?;
                    let reply = exchange(messages).await?;
                    if reorder && matches!(&reply, Some(RemoteReply::Result(reply)) if reply.seq == Some(1)) { futures_timer::Delay::new(Duration::from_millis(30)).await; }
                    serde_json::from_value(serde_json::to_value(reply).map_err(|e| HostError(e.to_string()))?).map_err(|e| HostError(e.to_string()))
                }.boxed()
            });
            serve_codemode_remote(request, wrapped, options).await;
            Ok(())
        }.boxed()
    });
    Ok(Arc::new(move || Ok(executor.clone())))
}
fn sandbox(backend: Backend, options: SandboxOptions) -> Result<CodemodeSandbox, HostError> {
    // Compilation is a process-wide prerequisite, not part of the short per-script deadline below.
    let wasm = load_quickjs_wasm(None)?;
    match backend {
        Backend::Worker => CodemodeSandbox::new(options),
        Backend::Inline => CodemodeSandbox::inline(options, wasm, Some(20_000)),
        Backend::Remote => CodemodeSandbox::remote(
            options,
            remote_factory(20_000, None, Arc::default(), false)?,
            Some(20_000),
        ),
    }
}
fn execute(sandbox: &CodemodeSandbox, code: &str) -> Result<CodemodeResult, HostError> {
    futures::executor::block_on(sandbox.execute(code, ExecuteOptions::default()))
}
fn value(result: &CodemodeResult, expected: Value) {
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(result.value(), Some(&expected), "{result:?}");
}
fn error(result: &CodemodeResult, kind: ErrorKind, name: Option<&str>, fragment: &str) {
    assert!(result.error().is_some(), "expected error, got {result:?}");
    let Some(error) = result.error() else { return };
    assert_eq!(error.kind, kind, "{result:?}");
    if let Some(name) = name {
        assert_eq!(error.name.as_deref(), Some(name), "{result:?}");
    }
    assert!(error.message.contains(fragment), "{result:?}");
}
fn basic(backend: Backend) -> Result<CodemodeSandbox, HostError> {
    let tools = vec![
        CodemodeTool::new("echo", |args, _| async move { Ok(args) }),
        CodemodeTool::new("add", |args, _| async move {
            let args = args.ok_or_else(|| HostError("missing add arguments".into()))?;
            Ok(Some(
                json!({"sum": args["a"].as_i64().unwrap_or(0) + args["b"].as_i64().unwrap_or(0)}),
            ))
        }),
        CodemodeTool::new("delay", |args, _| async move {
            futures_timer::Delay::new(Duration::from_millis(20)).await;
            Ok(args)
        }),
        CodemodeTool::new("fail", |_, _| async {
            Err(HostError("tool exploded".into()))
        }),
    ];
    sandbox(
        backend,
        SandboxOptions {
            tools,
            ..Default::default()
        },
    )
}
fn contract(backend: Backend, name: &str) -> TestResult {
    let s = basic(backend)?;
    match name {
        "returns_json" => {
            value(
                &execute(&s, "return {a:1,b:[true,'x']}")?,
                json!({"a":1,"b":[true,"x"]}),
            );
            value(&execute(&s, "return 'plain'")?, json!("plain"));
            let r = execute(&s, "")?;
            assert!(r.is_ok());
            assert_eq!(r.value(), None);
        }
        "top_level_await" => value(
            &execute(&s, "const x=await Promise.resolve(41);return x+1")?,
            json!(42),
        ),
        "ordered_output" => {
            let r = execute(
                &s,
                "console.log('hello',1,{a:1});text({json:true});text(undefined);text(7);image('data:image/jpeg;base64,iVBORw0KGgo=');image({image_url:'data:;base64,/9j/4A=='});image({type:'image',data:'R0lGODlh'});image('data:;base64,UklGRgAAAABXRUJQ');console.error(new Error('bad'))",
            )?;
            assert!(r.is_ok(), "{r:?}");
            let out = serde_json::to_value(r.output())?;
            assert_eq!(
                out[0],
                json!({"type":"text","text":"hello 1 {\"a\":1}","console":true})
            );
            assert_eq!(out[1]["text"], "{\"json\":true}");
            assert_eq!(out[2]["text"], "undefined");
            assert_eq!(out[3]["text"], "7");
            for (i, mime) in ["image/png", "image/jpeg", "image/gif", "image/webp"]
                .iter()
                .enumerate()
            {
                assert_eq!(out[i + 4]["mimeType"], *mime);
            }
            assert!(
                out[8]["text"]
                    .as_str()
                    .is_some_and(|t| t.starts_with("Error: bad"))
            );
        }
        "nonfinite_output" => {
            let r = execute(&s, "console.log('avg',0/0,1/0,-1/0,{a:1});text(0/0)")?;
            assert_eq!(
                serde_json::to_value(r.output())?,
                json!([{"type":"text","text":"avg NaN Infinity -Infinity {\"a\":1}","console":true},{"type":"text","text":"NaN"}])
            );
            error(&execute(&s, "throw NaN")?, ErrorKind::Script, None, "NaN");
        }
        "invalid_output_arguments" => {
            let r = execute(
                &s,
                r#"const a={};a.self=a;return [()=>text(a),()=>image(''),()=>image('https://example.com/a.png'),()=>image('data:image/png,raw'),()=>image({type:'text',text:'x'}),()=>image({type:'image',data:''}),()=>image(42),...['AAAA!','AAAAA','AA=A','','AAAA\n[Output truncated]','AAAA','QUJD','/9j/9w=='].map(d=>()=>image('data:;base64,'+d))].map(f=>{try{f();return 'accepted'}catch(e){return e.name+': '+e.message}})"#,
            )?;
            let a = r
                .value()
                .and_then(Value::as_array)
                .ok_or("missing error list")?;
            assert_eq!(a.len(), 15);
            assert!(
                a.iter()
                    .all(|v| v.as_str().is_some_and(|s| s.starts_with("TypeError:")))
            );
            assert!(a[0].as_str().is_some_and(|s| s.contains("circular")));
            assert!(
                a[2].as_str()
                    .is_some_and(|s| s.contains("remote image URLs"))
            );
        }
        "large_wrapped_image" => {
            let r = execute(
                &s,
                "image('data:;base64,iVBORw0K\\r\\nGgo=\\n'.replaceAll('\\\\r','\\r').replaceAll('\\\\n','\\n'));image('data:;base64,iVBORw0KGgoA'+'QUJD'.repeat(256*1024))",
            )?;
            assert!(r.is_ok(), "{r:?}");
            assert_eq!(r.output().len(), 2);
            assert!(matches!(&r.output()[1],OutputItem::Image{data,..} if data.len()>1<<20));
        }
        "exit_first_terminal" => {
            let r = execute(
                &s,
                "text('before');store('k',1);await tools.echo(1);try{exit()}catch{}text('after');return 'unreachable'",
            )?;
            assert!(r.is_ok(), "{r:?}");
            assert_eq!(r.value(), None);
            let wire = serde_json::to_value(r)?;
            assert_eq!(wire["output"], json!([{"type":"text","text":"before"}]));
            assert_eq!(wire["storeWrites"], json!({"set":{"k":1},"delete":[]}));
        }
        "partial_output" => {
            let r = execute(&s, "text('partial');throw new Error('boom')")?;
            error(&r, ErrorKind::Script, Some("Error"), "boom");
            assert_eq!(r.output().len(), 1);
        }
        "syntax_line" => {
            let r = execute(&s, "const a=1;\nconst b=;\nreturn a")?;
            error(&r, ErrorKind::Script, Some("SyntaxError"), "");
            assert!(
                r.error()
                    .and_then(|e| e.stack.as_ref())
                    .is_some_and(|s| s.contains("codemode.js:2"))
            );
        }
        "thrown_line" => {
            let r = execute(&s, "const a=1;\nthrow new TypeError('boom '+a)")?;
            error(&r, ErrorKind::Script, Some("TypeError"), "boom 1");
            assert!(
                r.error()
                    .and_then(|e| e.stack.as_ref())
                    .is_some_and(|s| s.contains("codemode.js:2"))
            );
        }
        "v8_stacks" => {
            let r = execute(
                &s,
                "console.log(new Error('inner'));\nthrow new RangeError('outer')",
            )?;
            let e = r.error().ok_or("missing error")?;
            let stack = e.stack.as_ref().ok_or("missing stack")?;
            assert!(stack.starts_with("RangeError: outer\n    at "), "{stack}");
            assert!(stack.contains("codemode.js:2"));
            assert!(!stack.contains("prelude"));
            assert!(
                matches!(&r.output()[0],OutputItem::Text{text,..} if text.starts_with("Error: inner\n    at ")&&text.contains("codemode.js:1")&&!text.contains("prelude"))
            );
        }
        "non_error_throws" => error(
            &execute(&s, "throw {code:7}")?,
            ErrorKind::Script,
            None,
            "{\"code\":7}",
        ),
        "nonserializable_return" => error(
            &execute(&s, "return 10n")?,
            ErrorKind::Script,
            Some("TypeError"),
            "",
        ),
        "sequential_tools" => {
            let r = execute(
                &s,
                "const a=await tools.add({a:1,b:2});const b=await tools.add({a:a.sum,b:10});return b.sum",
            )?;
            value(&r, json!(13));
            assert_eq!(r.calls().len(), 2);
            assert!(
                r.calls()
                    .iter()
                    .all(|c| c.name == "add" && c.status == CallStatus::Ok && c.duration_ms >= 0.)
            );
        }
        "concurrent_tools" => value(
            &execute(
                &s,
                "return {values:await Promise.all([tools.delay(1),tools.delay(2),tools.echo(3)]),names:Object.keys(tools)}",
            )?,
            json!({"values":[1,2,3],"names":["echo","add","delay","fail"]}),
        ),
        "normalized_aliases" => {
            let mut one = CodemodeTool::new("my-tool", |_, _| async { Ok(Some(json!("dash"))) });
            one.declaration.description = "Dashes".into();
            let two = CodemodeTool::new("my_tool", |_, _| async { Ok(Some(json!("underscore"))) });
            let three =
                CodemodeTool::new("mcp__docs__search", |_, _| async { Ok(Some(json!("mcp"))) });
            let s = sandbox(
                backend,
                SandboxOptions {
                    tools: vec![one, two, three],
                    ..Default::default()
                },
            )?;
            value(
                &execute(
                    &s,
                    "try{ALL_TOOLS.push({})}catch{}return [ALL_TOOLS,await tools.my_tool(),await tools['my-tool'](),await tools.mcp__docs__search()]",
                )?,
                json!([[{"name":"my_tool","description":"Dashes"},{"name":"mcp__docs__search","description":""}],"dash","dash","mcp"]),
            );
        }
        "undefined_and_null" => value(
            &execute(&s, "return [await tools.echo(),await tools.echo(null)]")?,
            json!([null, null]),
        ),
        "catchable_tool_errors" => {
            let r = execute(
                &s,
                "try{await tools.fail()}catch(e){return [e instanceof Error,e.message]}",
            )?;
            value(&r, json!([true, "tool exploded"]));
            assert_eq!(r.calls()[0].status, CallStatus::Error);
        }
        "unknown_tool" => error(
            &execute(&s, "return await tools.missing()")?,
            ErrorKind::Script,
            Some("TypeError"),
            "does not exist",
        ),
        "unawaited_cancellation" => {
            let token = Arc::new(Mutex::new(None));
            let seen = token.clone();
            let slow = CodemodeTool::new("slow", move |_, cx| {
                *seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.signal);
                async { futures::future::pending().await }
            });
            let s = sandbox(
                backend,
                SandboxOptions {
                    tools: vec![slow],
                    ..Default::default()
                },
            )?;
            let r = execute(&s, "tools.slow();return 'early'")?;
            value(&r, json!("early"));
            assert_eq!(r.calls()[0].status, CallStatus::Cancelled);
            assert!(
                token
                    .lock()
                    .map_err(|e| e.to_string())?
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
            );
        }
        "tool_hints" => {
            let r = execute(&s, "try{return tools.Echo}catch(e){return e.message}")?;
            value(
                &r,
                json!(
                    "tools.Echo does not exist. Did you mean tools.echo? ALL_TOOLS lists every tool; searchTools(query) finds tools by topic. Check for a member with \"Echo\" in tools."
                ),
            );
            value(
                &execute(
                    &s,
                    "return ['echo' in tools,'nothing' in tools,String(tools.toString),JSON.stringify(tools)]",
                )?,
                json!([true, false, "undefined", "{}"]),
            );
        }
        "registration" => {
            let t = CodemodeTool::new("new", |v, _| async move { Ok(v) });
            s.register_tool(t.clone())?;
            assert!(s.register_tool(t).is_err());
            value(&execute(&s, "return await tools.new('a')")?, json!("a"));
            assert!(s.unregister_tool("new")?);
            value(&execute(&s, "return 'new' in tools")?, json!(false));
        }
        "store_snapshot" => {
            let r=futures::executor::block_on(s.execute("const v=load('counter');store('counter',v+1);store('list',[1,{a:null}]);store('old',undefined);return [v,load('counter'),load('missing'),load('old')]",ExecuteOptions{store:[("counter".into(),json!(41)),("old".into(),json!("x"))].into_iter().collect(),..Default::default()}))?;
            value(&r, json!([41, 42, null, null]));
            assert_eq!(
                serde_json::to_value(r)?["storeWrites"],
                json!({"set":{"counter":42,"list":[1,{"a":null}]},"delete":["old"]})
            );
        }
        "store_copies" => {
            let r=futures::executor::block_on(s.execute("const v=load('obj');v.a=2;const kept={b:1};store('kept',kept);kept.b=2;return [load('obj').a,load('kept').b]",ExecuteOptions{store:[("obj".into(),json!({"a":1}))].into_iter().collect(),..Default::default()}))?;
            value(&r, json!([1, 1]));
        }
        "store_caps" => value(
            &execute(
                &s,
                "const f=fn=>{try{fn();return 'ok'}catch(e){return e.name}};return [f(()=>store(1,'x')),f(()=>load({})),f(()=>store('fn',()=>1)),f(()=>store('big','x'.repeat(300*1024))),f(()=>{for(let i=0;i<8;i++)store('k'+i,'x'.repeat(200*1024))})]",
            )?,
            json!([
                "TypeError",
                "TypeError",
                "TypeError",
                "RangeError",
                "RangeError"
            ]),
        ),
        "store_cap_hint" => {
            let r = execute(&s, "store('img','x'.repeat(300*1024))")?;
            error(
                &r,
                ErrorKind::Script,
                Some("RangeError"),
                "store(\"img\") value has 307202 characters of JSON",
            );
            assert!(
                r.error()
                    .is_some_and(|e| e.message.contains("Show images with image()"))
            );
        }
        "reserved_store_globals" => {
            for n in ["store", "load"] {
                assert!(
                    sandbox(
                        backend,
                        SandboxOptions {
                            globals: vec![CodemodeTool::new(n, |v, _| async move { Ok(v) })],
                            ..Default::default()
                        }
                    )
                    .is_err()
                );
            }
        }
        "top_level_globals" => {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let capture = seen.clone();
            let t = CodemodeTool::new("attach", move |v, _| {
                capture.lock().unwrap_or_else(|e| e.into_inner()).push(v);
                async { Ok(None) }
            });
            let s = sandbox(
                backend,
                SandboxOptions {
                    globals: vec![t],
                    ..Default::default()
                },
            )?;
            let r = execute(
                &s,
                "await attach({ref:1});attach('not awaited');return [typeof attach,typeof globalThis.attach]",
            )?;
            value(&r, json!(["function", "function"]));
            assert!(r.calls().is_empty());
            assert_eq!(
                *seen.lock().map_err(|e| e.to_string())?,
                vec![Some(json!({"ref":1})), Some(json!("not awaited"))]
            );
        }
        "namespaced_spread_globals" => {
            let mut t = CodemodeTool::new("models.search", |v, _| async move { Ok(v) });
            t.spread = true;
            let s = sandbox(
                backend,
                SandboxOptions {
                    globals: vec![t],
                    ..Default::default()
                },
            )?;
            value(
                &execute(
                    &s,
                    "return [await models.search('x',2),Object.isFrozen(models)]",
                )?,
                json!([["x", 2], true]),
            );
        }
        "namespace_hints" => {
            let t = CodemodeTool::new("files.read_text", |v, _| async move { Ok(v) });
            let s = sandbox(
                backend,
                SandboxOptions {
                    globals: vec![t],
                    ..Default::default()
                },
            )?;
            let r = execute(&s, "try{return files.readText}catch(e){return e.message}")?;
            assert!(
                r.value()
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.contains("files.read_text"))
            );
        }
        "global_validation" => {
            for names in [
                vec!["a", "a"],
                vec!["models", "models.list"],
                vec!["not-valid"],
                vec!["tools"],
                vec!["console"],
            ] {
                let globals = names
                    .into_iter()
                    .map(|n| CodemodeTool::new(n, |v, _| async move { Ok(v) }))
                    .collect();
                assert!(
                    sandbox(
                        backend,
                        SandboxOptions {
                            globals,
                            ..Default::default()
                        }
                    )
                    .is_err()
                );
            }
        }
        "sync_timeout" | "microtask_timeout" => {
            let now = Instant::now();
            let code = if name == "sync_timeout" {
                "while(true){}"
            } else {
                "while(true)await null"
            };
            let r = futures::executor::block_on(s.execute(
                code,
                ExecuteOptions {
                    timeout: Some(Some(Duration::from_millis(200))),
                    ..Default::default()
                },
            ))?;
            error(&r, ErrorKind::Timeout, None, "");
            if name == "sync_timeout" {
                assert!(now.elapsed() < Duration::from_secs(5));
            }
        }
        "infinite_deadline" => {
            let r = futures::executor::block_on(s.execute(
                "return await tools.delay('late')",
                ExecuteOptions {
                    timeout: Some(None),
                    ..Default::default()
                },
            ))?;
            value(&r, json!("late"));
        }
        "stalled_promise" => {
            error(
                &execute(
                    &s,
                    "await tools.echo(1);await new Promise(()=>{});return 'never'",
                )?,
                ErrorKind::Script,
                None,
                "can never settle",
            );
            value(
                &execute(&s, "tools.echo(2);return 'early'")?,
                json!("early"),
            );
        }
        "abort_and_close" => {
            futures::executor::block_on(async {
                let signal = CancellationToken::default();
                let abort = signal.clone();
                let (tx, mut rx) = mpsc::unbounded();
                let token = Arc::new(Mutex::new(None));
                let seen = token.clone();
                let t = CodemodeTool::new("hang", move |_, cx| {
                    *seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.signal);
                    let _ = tx.unbounded_send(());
                    async { futures::future::pending().await }
                });
                let s = sandbox(
                    backend,
                    SandboxOptions {
                        tools: vec![t],
                        ..Default::default()
                    },
                )?;
                let (r, ()) = futures::join!(
                    s.execute(
                        "await tools.hang();return 'never'",
                        ExecuteOptions {
                            signal,
                            ..Default::default()
                        }
                    ),
                    async {
                        rx.next().await;
                        abort.cancel("user cancelled");
                    }
                );
                error(&r?, ErrorKind::Aborted, None, "user cancelled");
                assert!(
                    token
                        .lock()
                        .map_err(|e| e.to_string())?
                        .as_ref()
                        .is_some_and(CancellationToken::is_cancelled)
                );
                let signal = CancellationToken::default();
                signal.cancel("pre-aborted");
                error(
                    &s.execute(
                        "return 1",
                        ExecuteOptions {
                            signal,
                            ..Default::default()
                        },
                    )
                    .await?,
                    ErrorKind::Aborted,
                    None,
                    "pre-aborted",
                );
                Ok::<_, Box<dyn std::error::Error>>(())
            })?;
        }
        "output_caps" => {
            for print in ["text(s)", "console.log(s)", "image('data:;base64,'+p)"] {
                let r = execute(
                    &s,
                    &format!(
                        "const s='x'.repeat(1<<20);const p='iVBORw0KGgoA'+'A'.repeat(1<<20);for(;;){{try{{{print}}}catch{{}}}}"
                    ),
                )?;
                error(
                    &r,
                    ErrorKind::Script,
                    Some("RangeError"),
                    "script output exceeded",
                );
                let chars: usize = r
                    .output()
                    .iter()
                    .map(|o| match o {
                        OutputItem::Text { text, .. } => text.len(),
                        OutputItem::Image { data, .. } => data.len(),
                    })
                    .sum();
                assert!(chars <= MAX_OUTPUT_CHARS && chars > MAX_OUTPUT_CHARS - (2 << 20));
            }
            let r = execute(&s, "for(;;)text('')")?;
            error(&r, ErrorKind::Script, Some("RangeError"), "");
            assert_eq!(r.output().len(), MAX_OUTPUT_ITEMS);
        }
        "closed_rejects" => {
            s.close();
            assert!(execute(&s, "return 1").is_err());
        }
        "parallel_isolation" => {
            let (a, b, c) = futures::executor::block_on(async {
                futures::join!(
                    s.execute(
                        "globalThis.shared='a';await null;return shared",
                        ExecuteOptions::default()
                    ),
                    s.execute(
                        "globalThis.shared='b';await null;return shared",
                        ExecuteOptions::default()
                    ),
                    s.execute("return typeof shared", ExecuteOptions::default())
                )
            });
            value(&a?, json!("a"));
            value(&b?, json!("b"));
            value(&c?, json!("undefined"));
        }
        "catchable_recursion" => value(
            &execute(
                &s,
                "let depth=0;function dive(){depth++;dive()}try{dive()}catch(e){return [e.name,depth>1000]}",
            )?,
            json!(["RangeError", true]),
        ),
        "no_host_globals" => value(
            &execute(
                &s,
                "return [typeof process,typeof require,typeof module,typeof setTimeout,typeof fetch,typeof WebAssembly,typeof std,typeof os,typeof globalThis.constructor]",
            )?,
            json!([
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                "function"
            ]),
        ),
        "confined_eval" => value(
            &execute(
                &s,
                "return [eval('typeof process'),new Function('return typeof process')(),tools.echo.constructor('return typeof require')(),(async()=>{}).constructor('return typeof setTimeout')() instanceof Promise]",
            )?,
            json!(["undefined", "undefined", "undefined", true]),
        ),
        "no_dynamic_import" => {
            let r = execute(
                &s,
                "try{await import('node:fs');return 'imported'}catch(e){return e.constructor.name}",
            )?;
            assert!(r.is_ok(), "{r:?}");
            assert_ne!(r.value(), Some(&json!("imported")));
        }
        "intrinsic_lockdown" => {
            let r = execute(
                &s,
                "Array.prototype.toJSON=()=>null;Object.prototype.toJSON=()=>5;Promise.prototype.then=()=>{};Map.prototype.get=()=>undefined;globalThis.JSON={stringify:()=>'x',parse:()=>'x'};store('k',[1]);return [await tools.echo([2]),JSON.stringify({a:1})]",
            )?;
            value(&r, json!([[2], "{\"a\":1}"]));
            assert_eq!(
                serde_json::to_value(r)?["storeWrites"]["set"],
                json!({"k":[1]})
            );
        }
        "instance_only_intrinsics" => value(
            &execute(
                &s,
                "return [Object.getPrototypeOf(function*(){}).prototype,Object.getPrototypeOf(async function(){}),Object.getPrototypeOf(async function*(){}).prototype,Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]())),Object.getPrototypeOf(Uint8Array.prototype)].map(Object.isFrozen)",
            )?,
            json!([true, true, true, true, true]),
        ),
        "instance_overrides" => value(
            &execute(
                &s,
                "const object={};object.toString=()=> 'custom';function Legacy(){}Legacy.prototype=Object.create(Error.prototype);Legacy.prototype.constructor=Legacy;const bare=new Error();bare.message='set later';class MyError extends Error{constructor(message){super(message);this.name='MyError'}}let patched='silent';try{Error.prototype.name='Patched'}catch(e){patched=e.constructor.name}return [String(object),new Legacy().constructor===Legacy,bare.message,new MyError('x').name,Error.prototype.name,patched]",
            )?,
            json!(["custom", true, "set later", "MyError", "Error", "TypeError"]),
        ),
        "nonstring_error_fields" => error(
            &execute(&s, "const e=new Error('x');e.message=42;throw e")?,
            ErrorKind::Script,
            Some("Error"),
            "42",
        ),
        "frozen_tools_console" => value(
            &execute(
                &s,
                "try{tools.echo=()=> 'nope'}catch{}try{tools.extra=()=> 'nope'}catch{}try{globalThis.tools=null}catch{}return ['extra' in tools,await tools.echo('still'),Object.isFrozen(console)]",
            )?,
            json!([false, "still", true]),
        ),
        _ => return Err(format!("unknown contract {name}").into()),
    }
    s.close();
    Ok(())
}
macro_rules! cases { ($backend:expr; $($name:ident),+ $(,)?) => { $(#[test] fn $name() -> TestResult { contract($backend,stringify!($name)) })+ }; }
macro_rules! backend_cases { ($name:ident,$backend:expr) => {mod $name {use super::*; cases!($backend;
    returns_json,top_level_await,ordered_output,nonfinite_output,invalid_output_arguments,large_wrapped_image,exit_first_terminal,partial_output,syntax_line,thrown_line,v8_stacks,non_error_throws,nonserializable_return,sequential_tools,concurrent_tools,normalized_aliases,undefined_and_null,catchable_tool_errors,unknown_tool,unawaited_cancellation,tool_hints,registration,store_snapshot,store_copies,store_caps,store_cap_hint,reserved_store_globals,top_level_globals,namespaced_spread_globals,namespace_hints,global_validation,sync_timeout,infinite_deadline,stalled_promise,microtask_timeout,abort_and_close,output_caps,closed_rejects,parallel_isolation,catchable_recursion,no_host_globals,confined_eval,no_dynamic_import,intrinsic_lockdown,instance_only_intrinsics,instance_overrides,nonstring_error_fields,frozen_tools_console
);}}; }
backend_cases!(worker, Backend::Worker);
backend_cases!(inline, Backend::Inline);
backend_cases!(remote, Backend::Remote);

const START: f64 = 1_768_471_200_000.;
#[derive(Default)]
struct Recorded {
    calls: Vec<RunCall>,
    events: Vec<String>,
    idles: Vec<During>,
    fingerprint: Option<String>,
}
type Answer = Arc<dyn Fn(&RunCall) -> Outcome + Send + Sync>;
fn recorded(
    sandbox: &CodemodeSandbox,
    code: &str,
    mut options: RunOptions,
    answer: Option<Answer>,
) -> Result<(CodemodeRun, Arc<Mutex<Recorded>>), HostError> {
    if options.tools.is_empty() {
        options.tools = vec![
            DeclaredTool {
                name: "echo".into(),
                ..Default::default()
            },
            DeclaredTool {
                name: "wait".into(),
                ..Default::default()
            },
        ];
    }
    let state = Arc::new(Mutex::new(Recorded::default()));
    let controller = Arc::new(Mutex::new(None::<RunController>));
    let seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let capture = state.clone();
    options.on_started = Some(Arc::new(move |s| {
        let mut state = capture.lock().unwrap_or_else(|e| e.into_inner());
        state.events.push("started".into());
        state.fingerprint = Some(s);
    }));
    let capture = state.clone();
    options.on_idle = Some(Arc::new(move |d| {
        let mut state = capture.lock().unwrap_or_else(|e| e.into_inner());
        state.events.push(format!("idle {}", d.seq));
        state.idles.push(d);
    }));
    let capture = state.clone();
    let control = controller.clone();
    options.on_call = Some(Arc::new(move |call| {
        {
            let mut state = capture.lock().unwrap_or_else(|e| e.into_inner());
            state.events.push(format!("call {}", call.id));
            state.calls.push(call.clone());
        }
        if call.name != "wait" {
            let seq = seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let answer = answer
                .as_ref()
                .map_or_else(|| Outcome::Ok(call.args.clone()), |answer| answer(&call));
            if let Some(control) = control.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                assert!(
                    control
                        .deliver(seq, START + 1000. * seq as f64, call.id, answer)
                        .is_ok()
                );
            }
        }
    }));
    let run = sandbox.start(code, options)?;
    *controller.lock().map_err(|e| HostError(e.to_string()))? = Some(run.controller());
    Ok((run, state))
}
fn run_options() -> RunOptions {
    RunOptions {
        started_at: START,
        seed: 1,
        ..Default::default()
    }
}
fn deterministic(backend: Backend, name: &str) -> TestResult {
    let s = sandbox(
        backend,
        SandboxOptions {
            memory_limit_bytes: Some(64 << 20),
            ..Default::default()
        },
    )?;
    futures::executor::block_on(async {
        match name {
            "replay_clock_random" => {
                let code = "const v=[Date.now(),performance.now(),Math.random(),Math.random(),new Date().toISOString()];const first=await tools.echo(v);text(JSON.stringify([Date.now(),performance.now(),Math.random()]));return await tools.echo(first.concat(Math.random()))";
                let (a, sa) = recorded(&s, code, run_options(), None)?;
                let (b, sb) = recorded(&s, code, run_options(), None)?;
                let (a, b) = futures::join!(a.promise, b.promise);
                let mut a = serde_json::to_value(a?)?;
                let mut b = serde_json::to_value(b?)?;
                a.as_object_mut().ok_or("not object")?.remove("calls");
                b.as_object_mut().ok_or("not object")?.remove("calls");
                assert_eq!(a, b);
                {
                    let ca = sa.lock().map_err(|e| e.to_string())?;
                    let cb = sb.lock().map_err(|e| e.to_string())?;
                    assert_eq!(ca.calls[0].args, cb.calls[0].args);
                    assert_eq!(
                        ca.calls[0].args.as_ref().ok_or("no arguments")?[0],
                        json!(START as u64)
                    );
                    assert_eq!(ca.calls[0].args.as_ref().ok_or("no arguments")?[1], 0);
                    assert_eq!(
                        ca.calls[0].args.as_ref().ok_or("no arguments")?[2],
                        json!(0.34227450605335275)
                    );
                    assert_eq!(
                        ca.calls[0].args.as_ref().ok_or("no arguments")?[4],
                        "2026-01-15T10:00:00.000Z"
                    );
                }
                let (zero, _) = recorded(
                    &s,
                    "return [Math.random(),Math.random()]",
                    RunOptions {
                        seed: 0,
                        ..run_options()
                    },
                    None,
                )?;
                value(
                    &zero.promise.await?,
                    json!([0.6711372530266764, 0.7258461452833667]),
                );
                let (later, _) = recorded(
                    &s,
                    "return Math.random()",
                    RunOptions {
                        started_at: START + 5000.,
                        ..run_options()
                    },
                    None,
                )?;
                value(&later.promise.await?, json!(0.34227450605335275));
                let (other, _) = recorded(
                    &s,
                    "return Math.random()",
                    RunOptions {
                        seed: 2,
                        ..run_options()
                    },
                    None,
                )?;
                assert_ne!(
                    other.promise.await?.value(),
                    Some(&json!(0.34227450605335275))
                );
            }
            "utc_without_weakrefs" => {
                let (r, _) = recorded(
                    &s,
                    "return [new Date(0).getHours(),new Date(0).getTimezoneOffset(),typeof WeakRef,typeof FinalizationRegistry]",
                    run_options(),
                    None,
                )?;
                value(&r.promise.await?, json!([0, 0, "undefined", "undefined"]));
            }
            "ordered_delivery" | "transport_reorders" => {
                let s = if name == "transport_reorders" {
                    CodemodeSandbox::remote(
                        SandboxOptions::default(),
                        remote_factory(50_000, None, Arc::default(), true)?,
                        Some(50_000),
                    )?
                } else {
                    s.clone()
                };
                let (run, state) = recorded(
                    &s,
                    "const order=[];await Promise.all([tools.wait('a').then(()=>order.push('a')),tools.wait('b').then(()=>order.push('b'))]);return order",
                    run_options(),
                    None,
                )?;
                let control = run.controller();
                let capture = state.clone();
                let (r, ()) = futures::join!(run.promise, async move {
                    futures::future::poll_fn(|cx| {
                        if capture
                            .lock()
                            .is_ok_and(|s| s.calls.len() == 2 && s.idles.len() == 1)
                        {
                            std::task::Poll::Ready(())
                        } else {
                            cx.waker().wake_by_ref();
                            std::task::Poll::Pending
                        }
                    })
                    .await;
                    if name == "transport_reorders" {
                        assert!(
                            control
                                .deliver(1, START + 1000., 1, Outcome::Ok(None))
                                .is_ok()
                        );
                        assert!(
                            control
                                .deliver(2, START + 2000., 2, Outcome::Ok(None))
                                .is_ok()
                        );
                    } else {
                        assert!(
                            control
                                .deliver(2, START + 2000., 1, Outcome::Ok(None))
                                .is_ok()
                        );
                        assert!(
                            control
                                .deliver(1, START + 1000., 2, Outcome::Ok(None))
                                .is_ok()
                        );
                    }
                });
                value(
                    &r?,
                    if name == "transport_reorders" {
                        json!(["a", "b"])
                    } else {
                        json!(["b", "a"])
                    },
                );
                assert_eq!(
                    state.lock().map_err(|e| e.to_string())?.idles,
                    vec![During { seq: 0, issued: 2 }, During { seq: 1, issued: 2 }]
                );
            }
            "fingerprint_idle_order" => {
                let (r, state) = recorded(
                    &s,
                    "await tools.echo(1);await Promise.all([tools.echo(2),tools.echo(3)])",
                    run_options(),
                    None,
                )?;
                assert!(r.promise.await?.is_ok());
                let state = state.lock().map_err(|e| e.to_string())?;
                assert_eq!(&state.events[..3], ["started", "call 1", "idle 0"]);
                assert_eq!(
                    state.idles,
                    vec![
                        During { seq: 0, issued: 1 },
                        During { seq: 1, issued: 3 },
                        During { seq: 2, issued: 3 }
                    ]
                );
                let f = state.fingerprint.as_ref().ok_or("missing fingerprint")?;
                assert!(f.starts_with("quickjs-wasi 3.6.2, quickjs "));
                assert!(f.contains("heap 67108864"));
                assert!(f.ends_with(if matches!(backend, Backend::Worker) {
                    "budget none"
                } else {
                    "budget 20000"
                }));
            }
            "globals_share_call_numbers" => {
                let (r, state) = recorded(
                    &s,
                    "const found=await search('x',2);try{await tools.echo('fail')}catch(e){text(e.message)}return found",
                    RunOptions {
                        globals: vec![guest::VmGlobal {
                            name: "search".into(),
                            spread: true,
                        }],
                        ..run_options()
                    },
                    Some(Arc::new(|c| {
                        if c.name == "search" {
                            Outcome::Ok(Some(json!(["found"])))
                        } else {
                            Outcome::Error {
                                message: "no".into(),
                                properties: None,
                            }
                        }
                    })),
                )?;
                let r = r.promise.await?;
                value(&r, json!(["found"]));
                assert_eq!(r.calls().len(), 1);
                assert_eq!(r.calls()[0].status, CallStatus::Error);
                let state = state.lock().map_err(|e| e.to_string())?;
                assert_eq!(state.calls[0].target, "global");
                assert_eq!(state.calls[0].id, 1);
                assert_eq!(state.calls[0].args, Some(json!(["x", 2])));
                assert_eq!(state.calls[1].id, 2);
            }
            "monotonic_validation"
            | "properties_validation"
            | "sequence_validation"
            | "finished_state"
            | "external_abort" => {
                let two = name == "sequence_validation";
                let (run, state) = recorded(
                    &s,
                    if two {
                        "await Promise.all([tools.wait(1),tools.wait(2)])"
                    } else if name == "external_abort" {
                        "try{await tools.wait(1)}catch{text('caught')}text('after')"
                    } else {
                        "const before=Date.now();await tools.wait(1);return [before,Date.now(),performance.now()]"
                    },
                    run_options(),
                    None,
                )?;
                let control = run.controller();
                let after = control.clone();
                let (r, ()) = futures::join!(run.promise, async move {
                    futures::future::poll_fn(|cx| {
                        if state
                            .lock()
                            .is_ok_and(|s| s.calls.len() == if two { 2 } else { 1 })
                        {
                            std::task::Poll::Ready(())
                        } else {
                            cx.waker().wake_by_ref();
                            std::task::Poll::Pending
                        }
                    })
                    .await;
                    assert!(!control.finished());
                    match name {
                        "monotonic_validation" => {
                            assert!(control.deliver(1, f64::NAN, 1, Outcome::Ok(None)).is_err());
                        }
                        "properties_validation" => {
                            for properties in [Value::Null, json!([1])] {
                                assert!(
                                    control
                                        .deliver(
                                            1,
                                            START,
                                            1,
                                            Outcome::Error {
                                                message: "x".into(),
                                                properties: Some(properties)
                                            }
                                        )
                                        .is_err()
                                );
                            }
                        }
                        "sequence_validation" => {
                            assert!(control.deliver(1, START, 99, Outcome::Ok(None)).is_err());
                            assert!(control.deliver(1, START, 1, Outcome::Ok(None)).is_ok());
                            assert!(control.deliver(1, START, 2, Outcome::Ok(None)).is_err());
                            assert!(control.deliver(0, START, 2, Outcome::Ok(None)).is_err());
                            assert!(control.deliver(2, START, 2, Outcome::Ok(None)).is_ok());
                            return;
                        }
                        "external_abort" => {
                            control.abort("diverged");
                            return;
                        }
                        _ => {}
                    }
                    assert!(
                        control
                            .deliver(1, START - 2000., 1, Outcome::Ok(None))
                            .is_ok()
                    );
                });
                let r = r?;
                assert!(after.finished());
                assert!(after.deliver(1, f64::NAN, 99, Outcome::Ok(None)).is_ok());
                if name == "external_abort" {
                    error(&r, ErrorKind::Aborted, None, "diverged");
                    assert!(r.output().is_empty());
                } else if two {
                    assert!(r.is_ok());
                } else {
                    value(&r, json!([START as u64, START as u64, 0]));
                }
            }
            "terminal_during" => {
                for (code, seq, issued, ok) in [
                    ("await tools.echo(1);return await tools.echo(2)", 2, 2, true),
                    (
                        "await tools.echo(1);await Promise.all([tools.echo(2),tools.echo(3)]);throw new Error('x')",
                        3,
                        3,
                        false,
                    ),
                    ("return 1", 0, 0, true),
                ] {
                    let (r, _) = recorded(&s, code, run_options(), None)?;
                    let r = r.promise.await?;
                    assert_eq!(r.is_ok(), ok);
                    assert_eq!(
                        serde_json::to_value(r)?["during"],
                        json!({"seq":seq,"issued":issued})
                    );
                }
                assert!(
                    serde_json::to_value(s.execute("return 1", ExecuteOptions::default()).await?)?
                        .get("during")
                        .is_none()
                );
            }
            "rejection_properties" => {
                let (r, _) = recorded(
                    &s,
                    "try{await tools.echo(1)}catch(e){return [e instanceof Error,e.message,e.name,e.structuredOutput]}",
                    run_options(),
                    Some(Arc::new(|_| Outcome::Error {
                        message: "denied".into(),
                        properties: Some(
                            json!({"structuredOutput":{"code":7},"name":"Ignored","message":"ignored"}),
                        ),
                    })),
                )?;
                value(&r.promise.await?, json!([true,"denied","Error",{"code":7}]));
            }
            "explicit_script_names" => {
                let (r, _) = recorded(
                    &s,
                    "return [typeof tools.renamed,Object.keys(tools),ALL_TOOLS.map(t=>t.name)]",
                    RunOptions {
                        tools: vec![DeclaredTool {
                            name: "my-tool".into(),
                            js_name: Some("renamed".into()),
                            ..Default::default()
                        }],
                        ..run_options()
                    },
                    None,
                )?;
                value(
                    &r.promise.await?,
                    json!(["function", ["renamed", "my-tool"], ["renamed"]]),
                );
                let result = s.start(
                    "",
                    RunOptions {
                        tools: vec![
                            DeclaredTool {
                                name: "a-b".into(),
                                ..Default::default()
                            },
                            DeclaredTool {
                                name: "a_b".into(),
                                ..Default::default()
                            },
                        ],
                        ..run_options()
                    },
                );
                assert!(matches!(result,Err(e) if e.0.contains("script names must be unique")));
            }
            _ => return Err(format!("unknown deterministic contract {name}").into()),
        }
        Ok(())
    })
}
macro_rules! det_tests {($backend:expr; $($name:ident),+)=>{$(#[test] fn $name()->TestResult{deterministic($backend,stringify!($name))})+};}
macro_rules! deterministic_cases {($module:ident,$backend:expr)=>{mod $module{use super::*;det_tests!($backend;replay_clock_random,utc_without_weakrefs,ordered_delivery,fingerprint_idle_order,globals_share_call_numbers,monotonic_validation,properties_validation,sequence_validation,finished_state,external_abort,terminal_during,rejection_properties,explicit_script_names);}};}
deterministic_cases!(deterministic_worker, Backend::Worker);
deterministic_cases!(deterministic_remote, Backend::Remote);
#[test]
fn transport_reorders() -> TestResult {
    deterministic(Backend::Remote, "transport_reorders")
}

#[test]
fn inline_interrupt_budgets_include_tool_waits() -> TestResult {
    let wasm = load_quickjs_wasm(None)?;
    let s = CodemodeSandbox::inline(
        SandboxOptions {
            tools: vec![CodemodeTool::new("tick", |_, _| async {
                Ok(Some(json!(1)))
            })],
            ..Default::default()
        },
        wasm,
        Some(1000),
    )?;
    for code in [
        "while(true){}",
        "while(true)await null",
        "for(;;){for(let i=0;i<1e5;i++);await tools.tick()}",
    ] {
        let r = futures::executor::block_on(s.execute(
            code,
            ExecuteOptions {
                timeout: Some(None),
                ..Default::default()
            },
        ))?;
        error(
            &r,
            ErrorKind::Timeout,
            None,
            "Execution exceeded its interrupt budget of 1000",
        );
        if code.contains("tick") {
            assert!(r.calls().len() > 1);
        }
    }
    Ok(())
}
#[test]
fn remote_batches_output_and_enforces_its_own_caps() -> TestResult {
    let batches = Arc::new(Mutex::new(Vec::new()));
    let s = CodemodeSandbox::remote(
        SandboxOptions::default(),
        remote_factory(100_000, None, batches.clone(), false)?,
        None,
    )?;
    let r = execute(&s, "for(let i=0;i<5000;i++)console.log(i);return 'ok'")?;
    value(&r, json!("ok"));
    assert_eq!(r.output().len(), 5000);
    assert!(matches!(&r.output()[4999],OutputItem::Text{text,console:true} if text=="4999"));
    assert!(batches.lock().map_err(|e| e.to_string())?.len() <= 6);
    let s = CodemodeSandbox::remote(
        SandboxOptions::default(),
        remote_factory(1000, Some(8 << 20), Arc::default(), false)?,
        Some(1_000_000_000),
    )?;
    error(
        &execute(&s, "while(true){}")?,
        ErrorKind::Timeout,
        None,
        "Execution exceeded its interrupt budget of 1000",
    );
    let r = execute(
        &s,
        "const parts=[];for(;;)parts.push('x'.repeat(1<<20)+parts.length)",
    )?;
    assert!(!r.is_ok());
    assert!(
        serde_json::to_string(&r)?.contains("out of memory"),
        "{r:?}"
    );
    Ok(())
}
#[test]
fn worker_heap_limit_is_real_and_catchable() -> TestResult {
    let s = sandbox(
        Backend::Worker,
        SandboxOptions {
            memory_limit_bytes: Some(8 << 20),
            ..Default::default()
        },
    )?;
    let r = execute(
        &s,
        "const parts=[];try{for(;;)parts.push('x'.repeat(1<<20)+parts.length)}catch(e){return [e.name,e.message,parts.length]}",
    )?;
    assert!(r.is_ok(), "{r:?}");
    assert!(r.value().and_then(Value::as_array).is_some_and(|v| {
        v[1].as_str().is_some_and(|s| s.contains("out of memory"))
            && v[2].as_u64().is_some_and(|n| n > 0 && n < 20)
    }));
    Ok(())
}
#[test]
fn remote_failure_modes_are_sandbox_errors() -> TestResult {
    let bad: RemoteExecutor =
        Arc::new(|_, _| async { Err(HostError("object reset".into())) }.boxed());
    let silent: RemoteExecutor = Arc::new(|_, _| async { Ok(()) }.boxed());
    for (factory, message) in [
        (
            Arc::new(move || Ok(bad.clone())) as RemoteFactory,
            "Remote sandbox failed: object reset",
        ),
        (
            Arc::new(move || Ok(silent.clone())) as RemoteFactory,
            "Remote sandbox returned before the script settled",
        ),
        (
            Arc::new(|| Err(HostError("no binding".into()))) as RemoteFactory,
            "Failed to reach the remote sandbox: no binding",
        ),
    ] {
        let s = CodemodeSandbox::remote(SandboxOptions::default(), factory, None)?;
        error(&execute(&s, "return 1")?, ErrorKind::Sandbox, None, message);
    }
    Ok(())
}

#[test]
fn failed_wasm_loading_does_not_reserve_worker_capacity() -> TestResult {
    let acquired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = acquired.clone();
    let s = CodemodeSandbox::new(SandboxOptions {
        wasm_path: Some("missing-quickjs-guest.wasm".into()),
        acquire_worker: Some(Arc::new(move || {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(|| {}))
        })),
        ..Default::default()
    })?;
    let result = execute(&s, "return 1")?;
    error(&result, ErrorKind::Sandbox, None, "Failed to load QuickJS:");
    assert_eq!(acquired.load(std::sync::atomic::Ordering::SeqCst), 0);
    Ok(())
}
