use pi_rs_codemode::*;
use serde_json::{Value, json};
fn sandbox() -> Result<CodemodeSandbox, HostError> {
    let echo = CodemodeTool::new("echo", |v, _| async move { Ok(v) });
    let add = CodemodeTool::new("add", |v, _| async move {
        let v = v.unwrap_or(Value::Null);
        Ok(Some(json!(
            v["a"].as_f64().unwrap_or(0.0) + v["b"].as_f64().unwrap_or(0.0)
        )))
    });
    CodemodeSandbox::new(SandboxOptions {
        tools: vec![echo, add],
        ..Default::default()
    })
}
macro_rules! returns {
    ($name:ident, $source:expr, $expected:expr) => {
        #[test]
        fn $name() -> Result<(), HostError> {
            futures::executor::block_on(async {
                let result = sandbox()?
                    .execute($source, ExecuteOptions::default())
                    .await?;
                assert!(result.is_ok(), "{result:?}");
                assert_eq!(result.value(), Some(&$expected));
                Ok(())
            })
        }
    };
}
returns!(
    returns_json_values,
    "return { answer: 42, a: [null, true] }",
    json!({"answer":42,"a":[null,true]})
);
returns!(
    awaits_promises,
    "return await Promise.resolve(42)",
    json!(42)
);
returns!(
    awaits_host_tool,
    "return await tools.echo('yes')",
    json!("yes")
);
returns!(
    sequential_tool_calls,
    "return await tools.add({a: await tools.add({a:2,b:3}),b:8})",
    json!(13)
);
returns!(
    parallel_tools_preserve_input_order,
    "return await Promise.all([tools.echo(1),tools.echo(2)])",
    json!([1, 2])
);
returns!(
    omitted_and_null_arguments_round_trip,
    "return [await tools.echo(),await tools.echo(null)]",
    json!([null, null])
);
returns!(
    console_numbers_and_json,
    "console.log('avg', NaN, Infinity, -Infinity, {a:1}); text(NaN); return 1",
    json!(1)
);
returns!(
    has_no_host_globals,
    "return [typeof process,typeof require,typeof module,typeof setTimeout,typeof fetch,typeof WebAssembly,typeof std,typeof os,typeof globalThis.constructor]",
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
    ])
);
returns!(
    eval_is_confined_to_realm,
    "return [eval('typeof process'),new Function('return typeof process')(),tools.echo.constructor('return typeof require')()]",
    json!(["undefined", "undefined", "undefined"])
);
returns!(
    intrinsic_patches_do_not_corrupt_bridge,
    "Array.prototype.toJSON=()=>null; Object.prototype.toJSON=()=>5; Promise.prototype.then=()=>{}; Map.prototype.get=()=>undefined; globalThis.JSON={stringify:()=> 'x',parse:()=> 'x'}; store('k',[1]); return [await tools.echo([2]), JSON.stringify({a:1})]",
    json!([[2], "{\"a\":1}"])
);
returns!(
    instance_overrides_survive_lockdown,
    "const o={};o.toString=()=> 'custom'; function L(){} L.prototype=Object.create(Error.prototype); L.prototype.constructor=L; const e=new Error();e.message='set later'; class E extends Error{constructor(m){super(m);this.name='MyError'}} let patched='silent';try{Error.prototype.name='Patched'}catch(e){patched=e.constructor.name} return [String(o),new L().constructor===L,e.message,new E('x').name,Error.prototype.name,patched]",
    json!(["custom", true, "set later", "MyError", "Error", "TypeError"])
);
returns!(
    tools_are_frozen,
    "try{tools.echo=()=> 'nope'}catch{} try{tools.extra=()=>1}catch{}try{globalThis.tools=null}catch{}return ['extra' in tools,await tools.echo('still')]",
    json!([false, "still"])
);
returns!(
    store_returns_detached_copies,
    "const k={b:1};store('k',k);k.b=2;const x=load('k');x.b=3;return load('k').b",
    json!(1)
);
returns!(
    store_rejects_invalid_keys,
    "const a=f=>{try{f();return 'ok'}catch(e){return e.name}};return [a(()=>store(1,'x')),a(()=>load({})),a(()=>store('fn',()=>1)),a(()=>store('big','x'.repeat(300*1024)))]",
    json!(["TypeError", "TypeError", "TypeError", "RangeError"])
);
returns!(
    tools_allow_then_to_json_and_prototype_probes,
    "return ['echo' in tools,'nothing' in tools,String(tools.toString),JSON.stringify(tools)]",
    json!([true, false, "undefined", "{}"])
);
returns!(
    tool_listing_is_frozen,
    "try{ALL_TOOLS.push({})}catch{}return ALL_TOOLS.map(t=>t.name)",
    json!(["echo", "add"])
);
returns!(
    dynamic_import_has_no_module_loader,
    "try{await import('node:fs');return 'imported'}catch(e){return true}",
    json!(true)
);
returns!(
    instance_only_intrinsics_are_frozen,
    "return [Object.getPrototypeOf(function*(){}).prototype,Object.getPrototypeOf(async function(){}),Object.getPrototypeOf(Int8Array).prototype,Object.getPrototypeOf([][Symbol.iterator]()),Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]())),Object.getPrototypeOf(new Map()[Symbol.iterator]()),Object.getPrototypeOf(/a/[Symbol.matchAll](''))].every(o=>Object.isFrozen(o))",
    json!(true)
);

#[test]
fn exit_and_output_are_first_terminal_wins() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let result = sandbox()?.execute("text('before');store('k',1);try{exit()}catch{}text('after');store('k',2);return 42", ExecuteOptions::default()).await?;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(result.value(), None);
        assert_eq!(
            result.output(),
            &[OutputItem::Text {
                text: "before".into(),
                console: false
            }]
        );
        if let CodemodeResult::Success { store_writes, .. } = result {
            assert_eq!(store_writes.set.get("k"), Some(&json!(1)));
        }
        Ok(())
    })
}
#[test]
fn never_settling_promise_reports_stall() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let result = sandbox()?
            .execute(
                "await tools.echo(1);await new Promise(()=>{})",
                ExecuteOptions {
                    timeout: Some(None),
                    ..Default::default()
                },
            )
            .await?;
        assert_eq!(result.error().map(|e| e.kind), Some(ErrorKind::Script));
        assert!(
            result
                .error()
                .is_some_and(|e| e.message.contains("can never settle"))
        );
        Ok(())
    })
}
#[test]
fn failure_preserves_partial_output_and_discards_writes() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let result = sandbox()?
            .execute(
                "text('partial');store('x',1);throw new TypeError('bad')",
                ExecuteOptions::default(),
            )
            .await?;
        assert_eq!(
            result.error().and_then(|e| e.name.as_deref()),
            Some("TypeError")
        );
        assert_eq!(result.output().len(), 1);
        Ok(())
    })
}
