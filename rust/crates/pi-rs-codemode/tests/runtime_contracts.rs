use pi_rs_codemode::*;
use serde_json::json;
fn run(code: &str) -> Result<CodemodeResult, HostError> {
    futures::executor::block_on(CodemodeSandbox::default().execute(code, ExecuteOptions::default()))
}
#[test]
fn image_sniffs_magic_and_preserves_output_order() -> Result<(), HostError> {
    let result = run(
        "text('first');image('data:image/jpeg;base64,iVBORw0KGgo=');console.log('last',NaN,Infinity,{a:1})",
    )?;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        result.output(),
        &[
            OutputItem::Text {
                text: "first".into(),
                console: false
            },
            OutputItem::Image {
                data: "iVBORw0KGgo=".into(),
                mime_type: "image/png".into()
            },
            OutputItem::Text {
                text: "last NaN Infinity {\"a\":1}".into(),
                console: true
            },
        ]
    );
    Ok(())
}
#[test]
fn image_accepts_supported_magic_and_wrapped_data() -> Result<(), HostError> {
    let result = run(
        "image({image_url:'data:;BASE64,/9j/4A=='});image({type:'image',data:'R0lGODlh'});image('data:;base64,UklGRgAAAABXRUJQ');image('data:;base64,iVBORw0K\\nGgo=');return 1",
    )?;
    assert!(result.is_ok(), "{result:?}");
    let types: Vec<_> = result
        .output()
        .iter()
        .filter_map(|v| match v {
            OutputItem::Image { mime_type, .. } => Some(mime_type.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        types,
        ["image/jpeg", "image/gif", "image/webp", "image/png"]
    );
    Ok(())
}
#[test]
fn image_rejects_remote_corrupt_and_wrong_type_inputs() -> Result<(), HostError> {
    let result = run(
        "return [null,[],1,'',{}, {type:'text'}, {type:'image',data:''}, {image_url:{}}, 'https://x', 'data:image/png,iVBORw0KGgo=', 'data:;base64,a', 'data:;base64,YWJj', 'data:;base64,/9j/9AAA'].map(v=>{try{image(v);return 'accepted'}catch(e){return e.name+': '+e.message}})",
    )?;
    assert!(result.is_ok(), "{result:?}");
    let values = result
        .value()
        .and_then(|v| v.as_array())
        .ok_or_else(|| HostError("missing errors".into()))?;
    assert_eq!(values.len(), 13);
    assert!(
        values
            .iter()
            .all(|v| v.as_str().is_some_and(|s| s.starts_with("TypeError: ")))
    );
    assert!(
        values[8]
            .as_str()
            .is_some_and(|s| s.contains("remote image URLs"))
    );
    assert!(
        values[10]
            .as_str()
            .is_some_and(|s| s.contains("not valid base64"))
    );
    assert!(values[11].as_str().is_some_and(|s| s.contains("not a PNG")));
    Ok(())
}
#[test]
fn timeout_stops_pending_host_calls_with_partial_output() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let tool = CodemodeTool::new("wait", |_, _| async {
            futures::future::pending::<Result<Option<serde_json::Value>, HostError>>().await
        });
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            tools: vec![tool],
            ..Default::default()
        })?;
        let result = sandbox
            .execute(
                "text('before');await tools.wait()",
                ExecuteOptions {
                    timeout: Some(Some(std::time::Duration::from_millis(100))),
                    ..Default::default()
                },
            )
            .await?;
        assert_eq!(result.error().map(|e| e.kind), Some(ErrorKind::Timeout));
        assert_eq!(result.output().len(), 1);
        assert_eq!(result.calls()[0].status, CallStatus::Cancelled);
        Ok(())
    })
}
#[test]
fn abort_stops_microtask_loops() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let signal = CancellationToken::default();
        let cancel = signal.clone();
        let (started, ready) = futures::channel::oneshot::channel();
        let started = std::sync::Arc::new(std::sync::Mutex::new(Some(started)));
        let tool = CodemodeTool::new("started", move |v, _| {
            let started = started.clone();
            async move {
                if let Ok(mut sender) = started.lock()
                    && let Some(sender) = sender.take()
                {
                    let _ = sender.send(());
                }
                Ok(v)
            }
        });
        let execute = CodemodeSandbox::new(SandboxOptions {
            tools: vec![tool],
            ..Default::default()
        })?;
        let (result, ()) = futures::join!(
            execute.execute(
                "text('started');await tools.started();for(;;)await Promise.resolve()",
                ExecuteOptions {
                    signal,
                    timeout: Some(None),
                    ..Default::default()
                }
            ),
            async move {
                let _ = ready.await;
                cancel.cancel("stop");
            }
        );
        let result = result?;
        assert_eq!(
            result.error().map(|e| (&e.kind, e.message.as_str())),
            Some((&ErrorKind::Aborted, "stop"))
        );
        assert_eq!(result.output().len(), 1);
        Ok(())
    })
}
#[test]
fn close_cancels_every_active_execution() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let sandbox = CodemodeSandbox::default();
        let code = "for(;;)await Promise.resolve()";
        let opts = || ExecuteOptions {
            timeout: Some(Some(std::time::Duration::from_secs(1))),
            ..Default::default()
        };
        let (a, b, ()) = futures::join!(
            sandbox.execute(code, opts()),
            sandbox.execute(code, opts()),
            async {
                futures_timer::Delay::new(std::time::Duration::from_millis(100)).await;
                sandbox.close();
            }
        );
        assert_eq!(a?.error().map(|e| e.kind), Some(ErrorKind::Aborted));
        assert_eq!(b?.error().map(|e| e.kind), Some(ErrorKind::Aborted));
        assert!(sandbox.execute("return 1", opts()).await.is_err());
        Ok(())
    })
}
#[test]
fn output_limits_are_terminal_even_if_caught() -> Result<(), HostError> {
    let result = run("text('before');try{text('x'.repeat(16*1024*1024))}catch{}return 'ignored'")?;
    assert_eq!(
        result.error().and_then(|e| e.name.as_deref()),
        Some("RangeError"),
        "{result:?}"
    );
    assert_eq!(result.output().len(), 1);
    Ok(())
}
#[test]
fn runtime_loop_budget_stops_sync_code() -> Result<(), HostError> {
    let sandbox = CodemodeSandbox::new(SandboxOptions {
        loop_iteration_limit: 1000,
        ..Default::default()
    })?;
    let result =
        futures::executor::block_on(sandbox.execute("for(;;){}", ExecuteOptions::default()))?;
    assert_eq!(result.error().map(|e| e.kind), Some(ErrorKind::Timeout));
    Ok(())
}
#[test]
fn custom_globals_preserve_spread_undefined_and_null() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let plain = CodemodeTool::new("plain", |v, _| async move {
            Ok(Some(json!({"omitted":v.is_none(),"value":v})))
        });
        let mut spread = CodemodeTool::new("ns.spread", |v, _| async move { Ok(v) });
        spread.spread = true;
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            globals: vec![plain, spread],
            ..Default::default()
        })?;
        let result=sandbox.execute("return [await plain(),await plain(null),await ns.spread(undefined,1),Object.isFrozen(ns)]",ExecuteOptions::default()).await?;
        assert_eq!(
            result.value(),
            Some(
                &json!([{"omitted":true,"value":null},{"omitted":false,"value":null},[null,1],true])
            )
        );
        assert!(result.calls().is_empty());
        Ok(())
    })
}
#[test]
fn guards_suggest_normalized_matches_in_tools_and_namespaces() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let tool = CodemodeTool::new("find_users", |v, _| async move { Ok(v) });
        let global = CodemodeTool::new("files.read_text", |v, _| async move { Ok(v) });
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            tools: vec![tool],
            globals: vec![global],
            ..Default::default()
        })?;
        let result=sandbox.execute("const get=f=>{try{f()}catch(e){return e.message}};return [get(()=>tools.findUsers),get(()=>files.readText),get(()=>tools.missing)]",ExecuteOptions::default()).await?;
        assert_eq!(
            result.value(),
            Some(&json!([
                "tools.findUsers does not exist. Did you mean tools.find_users? ALL_TOOLS lists every tool; searchTools(query) finds tools by topic. Check for a member with \"findUsers\" in tools.",
                "files.readText does not exist. Did you mean files.read_text? Check for a member with \"readText\" in files.",
                "tools.missing does not exist. Available: find_users. ALL_TOOLS lists every tool; searchTools(query) finds tools by topic. Check for a member with \"missing\" in tools."
            ]))
        );
        Ok(())
    })
}
