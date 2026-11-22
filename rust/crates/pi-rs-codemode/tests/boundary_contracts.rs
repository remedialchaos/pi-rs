use futures::{FutureExt, channel::oneshot};
use pi_rs_codemode::{guest::*, *};
use serde_json::json;
use std::{
    process::Command,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
type TestResult = Result<(), Box<dyn std::error::Error>>;

fn injected(message: VmMessage) -> Result<CodemodeResult, HostError> {
    let target: RemoteExecutor = Arc::new(move |_, exchange| {
        let message = message.clone();
        async move {
            exchange(vec![message]).await?;
            Ok(())
        }
        .boxed()
    });
    let sandbox = CodemodeSandbox::remote(
        SandboxOptions::default(),
        Arc::new(move || Ok(target.clone())),
        None,
    )?;
    futures::executor::block_on(sandbox.execute("return 1", ExecuteOptions::default()))
}
fn broken_bridge(
    value: Option<&str>,
    writes: Option<&str>,
    error: Option<&str>,
    reason: &str,
) -> TestResult {
    let result = injected(VmMessage::Done {
        ok: error.is_none(),
        value: value.map(str::to_owned),
        writes: writes.map(str::to_owned),
        error: error.map(str::to_owned),
        during: None,
    })?;
    let error = result.error().ok_or("broken bridge succeeded")?;
    assert_eq!(error.kind, ErrorKind::Sandbox);
    assert!(
        error
            .message
            .contains(&format!("Sandbox bridge broken: {reason}")),
        "{result:?}"
    );
    Ok(())
}
#[test]
fn bridge_rejects_non_array_writes() -> TestResult {
    broken_bridge(
        Some("1"),
        Some("null"),
        None,
        "store writes are not an array",
    )
}
#[test]
fn bridge_rejects_malformed_write_entries() -> TestResult {
    broken_bridge(
        Some("1"),
        Some("[1]"),
        None,
        "store writes contain a malformed entry",
    )
}
#[test]
fn bridge_rejects_invalid_stored_json() -> TestResult {
    broken_bridge(
        Some("1"),
        Some(r#"[["k","{"]]"#),
        None,
        "store value for \"k\" is not valid JSON",
    )
}
#[test]
fn bridge_rejects_invalid_return_json() -> TestResult {
    broken_bridge(
        Some("{"),
        Some("[]"),
        None,
        "return value is not valid JSON",
    )
}
#[test]
fn bridge_rejects_non_object_errors() -> TestResult {
    broken_bridge(None, None, Some("5"), "script error is not an object")
}
#[test]
fn bridge_rejects_errors_without_messages() -> TestResult {
    broken_bridge(None, None, Some("{}"), "script error is malformed")
}
#[test]
fn wire_rejects_unknown_worker_messages() {
    let error = serde_json::from_value::<VmMessage>(json!({"type":"nonsense"})).err();
    assert!(error.is_some_and(|error| error.to_string().contains("unknown variant")));
}
#[test]
fn bridge_retains_first_outcome_and_ignores_late_messages() -> TestResult {
    let target: RemoteExecutor = Arc::new(|_, exchange| {
        async move {
            exchange(vec![
                VmMessage::Done {
                    ok: true,
                    value: Some("1".into()),
                    writes: Some("[]".into()),
                    error: None,
                    during: None,
                },
                VmMessage::Crash {
                    message: "late crash".into(),
                },
                VmMessage::Output {
                    item: OutputItem::Text {
                        text: "late output".into(),
                        console: false,
                    },
                },
            ])
            .await?;
            Ok(())
        }
        .boxed()
    });
    let s = CodemodeSandbox::remote(
        SandboxOptions::default(),
        Arc::new(move || Ok(target.clone())),
        None,
    )?;
    let r = futures::executor::block_on(s.execute("", ExecuteOptions::default()))?;
    assert_eq!(r.value(), Some(&json!(1)));
    assert!(r.output().is_empty());
    Ok(())
}

fn cancellation_does_not_wait_for_exit(how: &str) -> TestResult {
    load_quickjs_wasm(None)?;
    futures::executor::block_on(async {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let released = Arc::new(AtomicBool::new(false));
        let (release_entered, release_started) = oneshot::channel();
        let release_entered = Arc::new(Mutex::new(Some(release_entered)));
        let gate_worker = gate.clone();
        let release_seen = released.clone();
        let exit = Arc::new(Mutex::new(None));
        let save_exit = exit.clone();
        let signal = Arc::new(Mutex::new(None));
        let save_signal = signal.clone();
        let (ready, called) = oneshot::channel();
        let ready = Arc::new(Mutex::new(Some(ready)));
        let s = CodemodeSandbox::new(SandboxOptions {
            timeout: Some(Duration::from_millis(200)),
            acquire_worker: Some(Arc::new(move || {
                let gate = gate_worker.clone();
                let released = release_seen.clone();
                let release_entered = release_entered.clone();
                Ok(Box::new(move || {
                    if let Some(entered) = release_entered
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                    {
                        let _ = entered.send(());
                    }
                    let (lock, condition) = &*gate;
                    let mut open = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while !*open {
                        open = condition.wait(open).unwrap_or_else(|e| e.into_inner());
                    }
                    released.store(true, Ordering::SeqCst);
                }))
            })),
            on_worker_started: Some(Arc::new(move |worker| {
                *save_exit.lock().map_err(|e| HostError(e.to_string()))? = Some(worker);
                Ok(())
            })),
            tools: vec![CodemodeTool::new("hang", move |_, context| {
                *save_signal.lock().unwrap_or_else(|e| e.into_inner()) = Some(context.signal);
                if let Some(ready) = ready.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = ready.send(());
                }
                futures::future::pending()
            })],
            ..Default::default()
        })?;
        let cancelled = CancellationToken::default();
        let execution = s.execute(
            "text('partial'); await tools.hang()",
            ExecuteOptions {
                signal: cancelled.clone(),
                ..Default::default()
            },
        );
        let settle = async {
            called.await?;
            if how == "abort" {
                cancelled.cancel("cancelled");
            }
            if how == "close" {
                s.close();
            }
            Ok::<(), Box<dyn std::error::Error>>(())
        };
        let started = Instant::now();
        let (r, settled) = futures::join!(execution, settle);
        // Always release the native worker, including when an assertion below fails.
        let worker = exit
            .lock()
            .map_err(|e| e.to_string())?
            .clone()
            .ok_or("missing worker")?;
        let entered = matches!(
            futures::future::select(
                release_started,
                futures_timer::Delay::new(Duration::from_secs(2))
            )
            .await,
            futures::future::Either::Left((Ok(()), _))
        );
        let pending_exit = worker.wait().now_or_never().is_none();
        let had_released = released.load(Ordering::SeqCst);
        let (lock, condition) = &*gate;
        *lock.lock().map_err(|e| e.to_string())? = true;
        condition.notify_all();
        worker.wait().await;
        settled?;
        let r = r?;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(entered, "worker did not reach its exit accounting hook");
        assert!(
            pending_exit && !had_released,
            "result waited for actual worker exit"
        );
        assert_eq!(
            r.error().map(|e| e.kind),
            Some(if how == "timeout" {
                ErrorKind::Timeout
            } else {
                ErrorKind::Aborted
            })
        );
        assert_eq!(r.calls()[0].status, CallStatus::Cancelled);
        assert_eq!(
            r.output(),
            &[OutputItem::Text {
                text: "partial".into(),
                console: false
            }]
        );
        assert!(
            signal
                .lock()
                .map_err(|e| e.to_string())?
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        );
        assert!(released.load(Ordering::SeqCst));
        s.close();
        Ok(())
    })
}
#[test]
fn abort_settles_without_waiting_for_actual_worker_exit() -> TestResult {
    cancellation_does_not_wait_for_exit("abort")
}
#[test]
fn timeout_settles_without_waiting_for_actual_worker_exit() -> TestResult {
    cancellation_does_not_wait_for_exit("timeout")
}
#[test]
fn close_settles_without_waiting_for_actual_worker_exit() -> TestResult {
    cancellation_does_not_wait_for_exit("close")
}
#[test]
fn deterministic_start_reports_actual_exit_and_releases_admission() -> TestResult {
    futures::executor::block_on(async {
        let released = Arc::new(AtomicBool::new(false));
        let seen = released.clone();
        let exit = Arc::new(Mutex::new(None));
        let save = exit.clone();
        let s = CodemodeSandbox::new(SandboxOptions {
            acquire_worker: Some(Arc::new(move || {
                let seen = seen.clone();
                Ok(Box::new(move || {
                    seen.store(true, Ordering::SeqCst);
                }))
            })),
            on_worker_started: Some(Arc::new(move |worker| {
                *save.lock().map_err(|e| HostError(e.to_string()))? = Some(worker);
                Ok(())
            })),
            ..Default::default()
        })?;
        let run = s.start("return 2", RunOptions::default())?;
        let control = run.controller();
        assert_eq!(run.promise.await?.value(), Some(&json!(2)));
        assert!(control.finished());
        let worker = exit
            .lock()
            .map_err(|e| e.to_string())?
            .clone()
            .ok_or("missing worker")?;
        worker.wait().await;
        assert!(released.load(Ordering::SeqCst));
        Ok(())
    })
}
#[test]
fn remote_abort_completes_the_server_and_disposes_its_vm() -> TestResult {
    futures::executor::block_on(async {
        let wasm = load_quickjs_wasm(None)?;
        let completed = Arc::new(AtomicBool::new(false));
        let seen = completed.clone();
        let executor: RemoteExecutor = Arc::new(move |request, exchange| {
            let wasm = wasm.clone();
            let seen = seen.clone();
            async move {
                serve_codemode_remote(
                    request,
                    exchange,
                    ServeOptions {
                        wasm,
                        max_interrupt_budget: None,
                        max_memory_limit_bytes: None,
                    },
                )
                .await;
                seen.store(true, Ordering::SeqCst);
                Ok(())
            }
            .boxed()
        });
        let (ready, called) = oneshot::channel();
        let ready = Arc::new(Mutex::new(Some(ready)));
        let s = CodemodeSandbox::remote(
            SandboxOptions {
                tools: vec![CodemodeTool::new("hang", move |_, _| {
                    if let Some(ready) = ready.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        let _ = ready.send(());
                    }
                    futures::future::pending()
                })],
                ..Default::default()
            },
            Arc::new(move || Ok(executor.clone())),
            None,
        )?;
        let signal = CancellationToken::default();
        let result = s.execute(
            "await tools.hang()",
            ExecuteOptions {
                signal: signal.clone(),
                ..Default::default()
            },
        );
        let cancel = async {
            called.await?;
            signal.cancel("stop");
            Ok::<(), oneshot::Canceled>(())
        };
        let (r, cancelled) = futures::join!(result, cancel);
        cancelled?;
        assert_eq!(r?.error().map(|e| e.kind), Some(ErrorKind::Aborted));
        assert!(
            completed.load(Ordering::SeqCst),
            "server was dropped with an unresolved exchange"
        );
        Ok(())
    })
}

#[test]
fn non_deterministic_execute_uses_real_clock_and_fresh_randomness() -> TestResult {
    load_quickjs_wasm(None)?;
    let s = CodemodeSandbox::default();
    let before = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let a = futures::executor::block_on(s.execute(
        "return [Date.now(),Math.random()]",
        ExecuteOptions::default(),
    ))?;
    let b = futures::executor::block_on(s.execute(
        "return [Date.now(),Math.random()]",
        ExecuteOptions::default(),
    ))?;
    let after = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let a = a.value().ok_or("missing first result")?;
    let b = b.value().ok_or("missing second result")?;
    assert!(
        a[0].as_u64()
            .is_some_and(|clock| (before..=after).contains(&clock))
    );
    assert!(
        b[0].as_u64()
            .is_some_and(|clock| (before..=after).contains(&clock))
    );
    assert_ne!(a[1], b[1]);
    Ok(())
}
#[test]
fn child_native_process_lifetime_and_timezone() -> TestResult {
    let mode = std::env::var("PI_CODEMODE_CHILD_MODE").unwrap_or_else(|_| "normal".into());
    load_quickjs_wasm(None)?;
    let s = CodemodeSandbox::default();
    if mode == "timezone" {
        let plain = futures::executor::block_on(s.execute(
            "return new Date(0).getTimezoneOffset()",
            ExecuteOptions::default(),
        ))?;
        assert_eq!(plain.value(), Some(&json!(-330)));
        let deterministic = s.start(
            "return [new Date().getTimezoneOffset(),typeof WeakRef,typeof FinalizationRegistry]",
            RunOptions::default(),
        )?;
        let result = futures::executor::block_on(deterministic.promise)?;
        assert_eq!(result.value(), Some(&json!([0, "undefined", "undefined"])));
    } else if mode == "normal" {
        let result =
            futures::executor::block_on(s.execute("return 42", ExecuteOptions::default()))?;
        assert_eq!(result.value(), Some(&json!(42)));
    } else {
        cancellation_does_not_wait_for_exit(&mode)?;
    }
    Ok(())
}
fn child_process(mode: &str) -> TestResult {
    let mut child = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "child_native_process_lifetime_and_timezone",
            "--nocapture",
        ])
        .env("PI_CODEMODE_CHILD_MODE", mode)
        .env("TZ", "Asia/Kolkata")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            assert!(
                output.status.success(),
                "child {mode}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            break;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            let _ = child.wait();
            return Err(format!("child {mode} did not exit promptly").into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}
#[test]
fn utc_determinism_does_not_depend_on_host_timezone() -> TestResult {
    child_process("timezone")
}
#[test]
fn native_process_exits_after_abort() -> TestResult {
    child_process("abort")
}
#[test]
fn native_process_exits_after_timeout() -> TestResult {
    child_process("timeout")
}
#[test]
fn native_process_exits_after_close() -> TestResult {
    child_process("close")
}

#[test]
fn json_transport_preserves_undefined_and_explicit_null() -> TestResult {
    let reply = VmReply {
        id: 1,
        ok: true,
        payload: None,
        seq: None,
        now: None,
        properties: None,
    };
    assert_eq!(
        serde_json::to_value(RemoteReply::Result(reply))?,
        json!({"type":"result","id":1,"ok":true})
    );
    let call = VmMessage::Call {
        id: 1,
        target: "tool".into(),
        name: "echo".into(),
        args: None,
    };
    assert_eq!(
        serde_json::to_value(call)?,
        json!({"type":"call","id":1,"target":"tool","name":"echo"})
    );
    let explicit = VmReply {
        id: 1,
        ok: true,
        payload: Some("null".into()),
        seq: None,
        now: None,
        properties: None,
    };
    assert_eq!(
        serde_json::to_value(RemoteReply::Result(explicit))?["payload"],
        "null"
    );
    Ok(())
}

#[test]
fn explicit_empty_wasm_path_is_not_the_embedded_cache_key() -> TestResult {
    load_quickjs_wasm(None)?;
    assert!(load_quickjs_wasm(Some(std::path::Path::new(""))).is_err());
    Ok(())
}

#[test]
fn explicit_wasm_path_executes_the_vendored_guest() -> TestResult {
    let s = CodemodeSandbox::new(SandboxOptions {
        wasm_path: Some(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/quickjs.wasm"),
        ),
        ..Default::default()
    })?;
    let result = futures::executor::block_on(s.execute("return 1", ExecuteOptions::default()))?;
    assert_eq!(result.value(), Some(&json!(1)));
    Ok(())
}

#[test]
fn source_exact_directive_contracts() -> Result<(), String> {
    for input in ["// @optionsx {}\ntext(1)", "text(\"// @options: {}\")"] {
        assert_eq!(
            source::parse_codemode_source(input.into())?["options"],
            json!({})
        );
    }
    for (input, line) in [
        ("// count things\n// @options: {}\ntext(1)", 2),
        ("\n  // @options: {}\ntext(1)", 2),
        ("text(1)\r\n// @options: {}", 2),
        ("// @options: {}\ntext(1)\n// @options: {}", 3),
    ] {
        assert!(source::parse_codemode_source(input.into()).err().is_some_and(|e| e.contains(&format!("The // @options: line must be the first line of the input; found one on line {line}"))));
    }
    for (input, fragment) in [
        ("", "Expected JavaScript source text (non-empty)"),
        ("  \n", "Expected JavaScript source text (non-empty)"),
        (
            "// @options:\ntext(1)",
            "@options must be a JSON object with supported fields",
        ),
        (
            "// @options: {timeout_ms:1}\ntext(1)",
            "@options must be valid JSON with supported fields",
        ),
        (
            "// @options: [1]\ntext(1)",
            "@options must be a JSON object with supported fields",
        ),
        (
            "// @options: {\"yield\":1}\ntext(1)",
            "@options only supports `max_output_tokens` and `timeout_ms`; got `yield`",
        ),
        (
            "// @options: {\"max_output_tokens\":1.5}\ntext(1)",
            "@options field `max_output_tokens` must be a non-negative safe integer",
        ),
        (
            "// @options: {\"timeout_ms\":0}\ntext(1)",
            "@options field `timeout_ms` must be a positive integer",
        ),
        (
            "// @options: {\"timeout_ms\":1}",
            "The @options line must be followed by JavaScript source on subsequent lines",
        ),
        (
            "// @options: {\"timeout_ms\":1}\n  \n",
            "The @options line must be followed by JavaScript source on subsequent lines",
        ),
    ] {
        assert!(
            source::parse_codemode_source(input.into())
                .err()
                .is_some_and(|e| e.contains(fragment)),
            "{input}: {fragment}"
        );
    }
    Ok(())
}
