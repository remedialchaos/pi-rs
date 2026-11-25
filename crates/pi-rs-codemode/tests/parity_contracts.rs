use pi_rs_codemode::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
fn run(code: &str) -> Result<CodemodeResult, HostError> {
    futures::executor::block_on(CodemodeSandbox::default().execute(code, ExecuteOptions::default()))
}
#[test]
fn thrown_errors_keep_script_line_and_v8_shaped_frames() -> Result<(), HostError> {
    let result = run("const a=1;\nthrow new TypeError('boom '+a)")?;
    let error = result
        .error()
        .ok_or_else(|| HostError(format!("expected error: {result:?}")))?;
    assert_eq!(
        (error.kind, error.name.as_deref(), error.message.as_str()),
        (ErrorKind::Script, Some("TypeError"), "boom 1")
    );
    let stack = error.stack.as_deref().unwrap_or("");
    assert!(stack.starts_with("TypeError: boom 1\n    at "), "{stack}");
    assert!(stack.contains("codemode.js:2"), "{stack}");
    Ok(())
}
#[test]
fn console_error_stacks_keep_script_line() -> Result<(), HostError> {
    let result = run("console.log(new Error('inner'));\nthrow new RangeError('outer')")?;
    assert!(
        result
            .error()
            .and_then(|e| e.stack.as_deref())
            .is_some_and(|s| s.contains("codemode.js:2")),
        "{result:?}"
    );
    assert!(
        matches!(&result.output()[0],OutputItem::Text{text,console:true} if text.starts_with("Error: inner\n    at ")&&text.contains("codemode.js:1"))
    );
    Ok(())
}
#[test]
fn syntax_errors_keep_the_original_source_line() -> Result<(), HostError> {
    let result = run("const a=1;\nconst b=;\nreturn a")?;
    let error = result
        .error()
        .ok_or_else(|| HostError("expected syntax error".into()))?;
    assert_eq!(
        (error.kind, error.name.as_deref()),
        (ErrorKind::Script, Some("SyntaxError"))
    );
    assert!(
        error
            .stack
            .as_deref()
            .is_some_and(|s| s.contains("codemode.js:2")),
        "{result:?}"
    );
    Ok(())
}
#[test]
fn nonserializable_returns_are_script_type_errors() -> Result<(), HostError> {
    for code in ["return 10n", "const a={};a.a=a;return a"] {
        let result = run(code)?;
        assert_eq!(
            result.error().map(|e| (e.kind, e.name.as_deref())),
            Some((ErrorKind::Script, Some("TypeError"))),
            "{result:?}"
        );
    }
    Ok(())
}
#[test]
fn thrown_non_errors_use_json_or_primitive_display() -> Result<(), HostError> {
    for (code, message) in [
        ("throw {code:7}", "{\"code\":7}"),
        ("throw NaN", "NaN"),
        ("throw null", "null"),
        ("throw undefined", "undefined"),
        ("const e=new Error('x');e.message=42;throw e", "42"),
    ] {
        assert_eq!(
            run(code)?.error().map(|e| e.message.clone()),
            Some(message.into())
        );
    }
    Ok(())
}
#[test]
fn rejected_argument_serialization_is_an_async_rejection() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let echo = CodemodeTool::new("echo", |v, _| async move { Ok(v) });
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            tools: vec![echo],
            ..Default::default()
        })?;
        let result = sandbox
            .execute(
                "const p=tools.echo(1n);return [p instanceof Promise,await p.catch(e=>e.name)]",
                ExecuteOptions::default(),
            )
            .await?;
        assert_eq!(
            result.value(),
            Some(&json!([true, "TypeError"])),
            "{result:?}"
        );
        assert!(result.calls().is_empty());
        Ok(())
    })
}
#[test]
fn tool_failures_are_catchable_and_record_error_status() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let fail = CodemodeTool::new("fail", |_, _| async move {
            Err(HostError("tool exploded".into()))
        });
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            tools: vec![fail],
            ..Default::default()
        })?;
        let result=sandbox.execute("try{await tools.fail()}catch(e){return {isError:e instanceof Error,message:e.message}}",ExecuteOptions::default()).await?;
        assert_eq!(
            result.value(),
            Some(&json!({"isError":true,"message":"tool exploded"}))
        );
        assert_eq!(result.calls()[0].status, CallStatus::Error);
        Ok(())
    })
}
#[test]
fn normalized_tool_collisions_keep_first_alias_and_listing() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let mut dash = CodemodeTool::new("my-tool", |_, _| async move { Ok(Some(json!("dash"))) });
        dash.declaration.description = "Dashes".into();
        let underscore =
            CodemodeTool::new(
                "my_tool",
                |_, _| async move { Ok(Some(json!("underscore"))) },
            );
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            tools: vec![dash, underscore],
            ..Default::default()
        })?;
        let result = sandbox
            .execute(
                "return [await tools.my_tool(),await tools['my-tool'](),ALL_TOOLS]",
                ExecuteOptions::default(),
            )
            .await?;
        assert_eq!(
            result.value(),
            Some(&json!(["dash","dash",[{"name":"my_tool","description":"Dashes"}]]))
        );
        assert!(result.calls().iter().all(|c| c.name == "my-tool"));
        Ok(())
    })
}
#[test]
fn unawaited_calls_are_started_then_cancelled_on_return() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let saved = Arc::new(Mutex::new(None));
        let target = saved.clone();
        let slow = CodemodeTool::new("slow", move |_, ctx| {
            if let Ok(mut saved) = target.lock() {
                *saved = Some(ctx.signal);
            }
            futures::future::pending::<Result<Option<Value>, HostError>>()
        });
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            tools: vec![slow],
            ..Default::default()
        })?;
        let result = sandbox
            .execute("tools.slow();return 'early'", ExecuteOptions::default())
            .await?;
        assert_eq!(result.value(), Some(&json!("early")));
        assert_eq!(result.calls()[0].status, CallStatus::Cancelled);
        assert!(
            saved
                .lock()
                .map_err(|e| HostError(e.to_string()))?
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        );
        Ok(())
    })
}
#[test]
fn tool_registration_changes_only_later_execution_snapshots() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let sandbox = CodemodeSandbox::default();
        let echo = CodemodeTool::new("echo", |v, _| async move { Ok(v) });
        sandbox.register_tool(echo.clone())?;
        assert!(sandbox.register_tool(echo).is_err());
        assert_eq!(
            sandbox
                .execute("return await tools.echo('a')", ExecuteOptions::default())
                .await?
                .value(),
            Some(&json!("a"))
        );
        assert!(sandbox.unregister_tool("echo")?);
        assert!(!sandbox.unregister_tool("echo")?);
        assert_eq!(
            sandbox
                .execute("return 'echo' in tools", ExecuteOptions::default())
                .await?
                .value(),
            Some(&json!(false))
        );
        Ok(())
    })
}
#[test]
fn store_snapshot_and_last_writes_are_detached_and_committed_on_success() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let store =
            indexmap::IndexMap::from([("counter".into(), json!(41)), ("old".into(), json!("x"))]);
        let result=CodemodeSandbox::default().execute("const seen=load('counter');store('counter',seen+1);store('list',[1,{a:null}]);store('old',undefined);return [seen,load('counter'),load('missing'),load('old')]",ExecuteOptions{store,..Default::default()}).await?;
        assert_eq!(result.value(), Some(&json!([41, 42, null, null])));
        if let CodemodeResult::Success { store_writes, .. } = result {
            assert_eq!(store_writes.set["counter"], json!(42));
            assert_eq!(store_writes.delete, ["old"]);
        } else {
            return Err(HostError("store execution failed".into()));
        }
        Ok(())
    })
}
#[test]
fn store_total_counts_keys_and_releases_deleted_space() -> Result<(), HostError> {
    let result = run(
        "const key='k'.repeat(1024*1024);let error;try{store(key,1)}catch(e){error=e.name}store('x',1);store('x',undefined);store('x',2);return [error,load('x')]",
    )?;
    assert_eq!(result.value(), Some(&json!(["RangeError", 2])));
    Ok(())
}
#[test]
fn custom_global_names_validate_reserved_and_namespace_conflicts() {
    for name in [
        "a.b.c",
        "a.",
        ".a",
        "tools.x",
        "store.x",
        "a.not-valid",
        "not-valid",
        "tools",
        "console",
        "store",
        "load",
        "globalThis",
    ] {
        let tool = CodemodeTool::new(name, |v, _| async move { Ok(v) });
        assert!(
            CodemodeSandbox::new(SandboxOptions {
                globals: vec![tool],
                ..Default::default()
            })
            .is_err(),
            "{name}"
        );
    }
    let tool = |name| CodemodeTool::new(name, |v, _| async move { Ok(v) });
    assert!(
        CodemodeSandbox::new(SandboxOptions {
            globals: vec![tool("models"), tool("models.list")],
            ..Default::default()
        })
        .is_err()
    );
}
#[test]
fn realms_do_not_share_globals_even_when_executions_overlap() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let sandbox = CodemodeSandbox::default();
        let (a, b, c) = futures::join!(
            sandbox.execute(
                "globalThis.shared='a';await null;return shared",
                ExecuteOptions::default()
            ),
            sandbox.execute(
                "globalThis.shared='b';await null;return shared",
                ExecuteOptions::default()
            ),
            sandbox.execute("return typeof shared", ExecuteOptions::default())
        );
        assert_eq!(
            [
                a?.value().cloned(),
                b?.value().cloned(),
                c?.value().cloned()
            ],
            [Some(json!("a")), Some(json!("b")), Some(json!("undefined"))]
        );
        Ok(())
    })
}
#[test]
fn image_accepts_large_wrapped_base64_without_mime_trust() -> Result<(), HostError> {
    let data = format!("iVBORw0KGgoA{}", "QUJD".repeat(256 * 1024));
    let code = format!("image('data:application/octet-stream;base64,{data}')");
    let result = run(&code)?;
    assert_eq!(
        result.output(),
        &[OutputItem::Image {
            data,
            mime_type: "image/png".into()
        }]
    );
    Ok(())
}
