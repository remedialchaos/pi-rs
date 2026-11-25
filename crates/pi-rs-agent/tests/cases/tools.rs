use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::{
    sync::Notify,
    time::{sleep, timeout},
};

fn ctx(tools: Vec<Arc<dyn AgentTool>>) -> AgentContext {
    AgentContext {
        tools,
        ..AgentContext::default()
    }
}
fn go() -> Vec<AgentMessage> {
    vec![AgentMessage::user("go")]
}
fn echo_call(id: &str, value: &str) -> ToolCall {
    call(id, "echo", [("value", json!(value))])
}
fn ids(events: &[AgentEvent], start: bool) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match (event, start) {
            (AgentEvent::ToolExecutionStart { tool_call_id, .. }, true)
            | (AgentEvent::ToolExecutionEnd { tool_call_id, .. }, false) => {
                Some(tool_call_id.as_str())
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn parallel_preflights_in_order_finishes_by_completion_and_commits_in_source_order()
-> TestResult {
    let release = Arc::new(Notify::new());
    let preflight = Arc::new(Mutex::new(Vec::new()));
    let observed = preflight.clone();
    let tool_release = release.clone();
    let tool = MockTool::new("echo", json!({"type":"object"}), move |id, _, _, _| {
        let release = tool_release.clone();
        let observed = observed.clone();
        Box::pin(async move {
            assert_eq!(*locked(&observed), ["one", "two"]);
            if id == "one" {
                release.notified().await;
            }
            Ok(AgentToolResult::text(id))
        })
    });
    let mut config = AgentLoopConfig::new(model());
    config.hooks.tool_calls.before_tool_call = Some(Arc::new(move |call, _| {
        locked(&preflight).push(call.tool_call.id.clone());
        Box::pin(std::future::ready(Ok(None)))
    }));
    let (provider, _) = scripted([
        tool_reply(vec![echo_call("one", "a"), echo_call("two", "b")]),
        answer("ok"),
    ]);
    let mut stream = agent_loop(
        go(),
        ctx(vec![Arc::new(tool)]),
        config,
        None,
        Some(provider),
    )?;
    let mut endings = Vec::new();
    let mut artifacts = Vec::new();
    while let Some(event) = timeout(Duration::from_secs(2), stream.next()).await? {
        match event {
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                if tool_call_id == "two" {
                    release.notify_one();
                }
                endings.push(tool_call_id);
                assert!(artifacts.is_empty());
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult(result),
            } => artifacts.push(result.tool_call_id.clone()),
            _ => {}
        }
    }
    assert_eq!(endings, ["two", "one"]);
    assert_eq!(artifacts, ["one", "two"]);
    assert_eq!(
        tool_texts(&stream.result().await?),
        [("one", "one".into(), false), ("two", "two".into(), false)]
    );
    Ok(())
}

#[tokio::test]
async fn steering_during_tools_waits_for_the_full_batch_even_in_sequential_mode() -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let mut tool = MockTool::new("echo", json!({"type":"object"}), move |id, _, _, _| {
        locked(&seen).push(id.to_owned());
        Box::pin(std::future::ready(Ok(AgentToolResult::text(id))))
    });
    // The per-tool override forces the entire turn to run sequentially despite a parallel default.
    tool.mode = Some(ToolExecutionMode::Sequential);
    let polled = AtomicUsize::new(0);
    let mut config = AgentLoopConfig::new(model());
    config.get_steering_messages = Some(Arc::new(move || {
        let result = if !locked(&calls).is_empty() && polled.fetch_add(1, Ordering::Relaxed) == 0 {
            vec![AgentMessage::user("interrupt")]
        } else {
            Vec::new()
        };
        Box::pin(std::future::ready(result))
    }));
    let (provider, requests) = scripted([
        tool_reply(vec![echo_call("one", "a"), echo_call("two", "b")]),
        answer("ok"),
    ]);
    let (events, messages) = collect(agent_loop(
        go(),
        ctx(vec![Arc::new(tool)]),
        config,
        None,
        Some(provider),
    )?)
    .await?;
    assert_eq!(requests.users(), [vec!["go"], vec!["go", "interrupt"]]);
    assert_eq!(
        tool_texts(&messages),
        [("one", "one".into(), false), ("two", "two".into(), false)]
    );
    let projected: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                Some(format!("start:{tool_call_id}"))
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                Some(format!("end:{tool_call_id}"))
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult(tool),
            } => Some(format!("result:{}", tool.tool_call_id)),
            AgentEvent::MessageStart {
                message: AgentMessage::User(user),
            } => match &user.content {
                StringOrBlocks::String(input) if input == "interrupt" => Some(input.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        projected,
        [
            "start:one",
            "end:one",
            "result:one",
            "start:two",
            "end:two",
            "result:two",
            "interrupt"
        ]
    );
    Ok(())
}

#[tokio::test]
async fn abort_answers_every_call_and_never_starts_unapproved_tools() -> TestResult {
    for mode in [ToolExecutionMode::Sequential, ToolExecutionMode::Parallel] {
        let signal = CancellationToken::new();
        let mut config = AgentLoopConfig::new(model());
        config.tool_execution = mode;
        let hook_calls = Arc::new(Mutex::new(Vec::new()));
        let observed = hook_calls.clone();
        config.hooks.tool_calls.before_tool_call = Some(Arc::new(move |context, signal| {
            locked(&observed).push(context.tool_call.id.clone());
            if mode == ToolExecutionMode::Parallel && context.tool_call.id == "2" {
                signal.cancel();
            }
            Box::pin(std::future::ready(Ok(None)))
        }));
        let invoked = Arc::new(Mutex::new(Vec::new()));
        let observed = invoked.clone();
        let tool = MockTool::new("echo", json!({"type":"object"}), move |id, _, signal, _| {
            locked(&observed).push(id.to_owned());
            signal.cancel();
            Box::pin(std::future::ready(Err(AgentError::Callback(
                "interrupted".into(),
            ))))
        });
        let (provider, _) = scripted([
            tool_reply(vec![
                echo_call("1", "a"),
                echo_call("2", "b"),
                echo_call("3", "c"),
            ]),
            reply(Vec::new(), StopReason::Aborted),
        ]);
        let (events, messages) = collect(agent_loop(
            go(),
            ctx(vec![Arc::new(tool)]),
            config,
            Some(signal),
            Some(provider),
        )?)
        .await?;
        let first = if mode == ToolExecutionMode::Sequential {
            assert_eq!(*locked(&invoked), ["1"]);
            assert_eq!(*locked(&hook_calls), ["1"]);
            "interrupted"
        } else {
            assert!(locked(&invoked).is_empty());
            assert_eq!(*locked(&hook_calls), ["1", "2"]);
            "Operation aborted"
        };
        assert_eq!(ids(&events, true), ["1", "2", "3"]);
        let mut ended = ids(&events, false);
        ended.sort_unstable();
        assert_eq!(ended, ["1", "2", "3"]);
        assert_eq!(
            tool_texts(&messages),
            [
                ("1", first.into(), true),
                ("2", "Operation aborted".into(), true),
                ("3", "Operation aborted".into(), true)
            ]
        );
    }
    Ok(())
}

#[tokio::test]
async fn length_stop_never_executes_tool_arguments_that_happen_to_validate() -> TestResult {
    let mut first = tool_reply(vec![echo_call("first", "parseable but incomplete")]);
    first.stop_reason = StopReason::Length;
    let (stream, requests) = scripted([first, answer("recovered")]);
    let (events, messages) = collect(agent_loop(
        go(),
        ctx(vec![Arc::new(MockTool::echo("echo", true))]),
        AgentLoopConfig::new(model()),
        None,
        Some(stream),
    )?)
    .await?;
    assert_eq!(requests.users().len(), 2);
    assert!(
        matches!(tool_texts(&messages).as_slice(), [("first", error, true)] if error.contains("output token limit"))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionEnd {
            duration_ms: None,
            is_error: true,
            ..
        }
    )));
    Ok(())
}

#[tokio::test]
async fn schema_preparation_coercion_and_hook_mutation_do_not_change_raw_call() -> TestResult {
    let schema = json!({"~kind":"Object","type":"object","properties":{
        "count":{"type":"integer"},"mode":{"anyOf":[{"const":"fast"},{"const":"deep"}]}},
        "required":["count"],"additionalProperties":false});
    let mut tool = MockTool::new("example", schema, |_, args, _, update| {
        update(AgentToolResult::text("working"));
        Box::pin(async move {
            Ok(AgentToolResult {
                structured_content: Some(args.clone()),
                ..AgentToolResult::text("ok")
            })
        })
    });
    tool.prepare = Some(Box::new(|args| {
        if let Some(old) = args.remove("amount") {
            args.insert("count".into(), old);
        }
        Ok(())
    }));
    let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(tool)];
    let context = ctx(tools);
    let assistant = answer("tool request");
    let signal = CancellationToken::new();
    let hooks_called = Arc::new(AtomicUsize::new(0));
    let seen = hooks_called.clone();
    let hooks = ToolCallHooks {
        before_tool_call: Some(Arc::new(move |context, _| {
            seen.fetch_add(1, Ordering::Relaxed);
            assert_eq!(context.args["count"].as_f64(), Some(42.0));
            assert_eq!(context.tool_call.arguments["amount"], "42");
            // Hooks are trusted to change already validated arguments, without revalidation.
            context.args["count"] = json!("approved override");
            Box::pin(std::future::ready(Ok(None)))
        })),
        after_tool_call: None,
    };
    let updates = Mutex::new(Vec::new());
    let on_update = |partial: AgentToolResult| {
        locked(&updates).push(input_text(&partial.content));
        Box::pin(std::future::ready(Ok(()))) as _
    };
    let run = |on_update| RunToolCallOptions {
        tools: &context.tools,
        assistant_message: &assistant,
        context: &context,
        hooks: &hooks,
        signal: &signal,
        on_update,
    };
    let valid = call(
        "valid",
        "example",
        [("amount", json!("42")), ("mode", json!("fast"))],
    );
    let success = run_tool_call(&valid, run(Some(&on_update))).await;
    assert!(!success.is_error);
    assert_eq!(
        success.result.structured_content,
        Some(json!({"mode":"fast","count":"approved override"}))
    );
    assert_eq!(success.tool_call, valid);
    let rejected = run_tool_call(
        &call("invalid", "example", [("amount", json!({"bad":true}))]),
        run(None),
    )
    .await;
    assert!(rejected.is_error);
    assert!(
        input_text(&rejected.result.content)
            .contains("Validation failed for tool \"example\":\n  - count:")
    );
    let mut broken = valid.clone();
    broken.arguments_error = Some("unfinished string".into());
    assert_eq!(
        input_text(&run_tool_call(&broken, run(None)).await.result.content),
        "Invalid arguments for tool \"example\": unfinished string"
    );
    assert_eq!(
        input_text(
            &run_tool_call(&call("absent", "missing", []), run(None))
                .await
                .result
                .content
        ),
        "Tool missing not found"
    );
    assert_eq!(*locked(&updates), ["working"]);
    assert_eq!(hooks_called.load(Ordering::Relaxed), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn hook_overrides_preserve_unreplaced_fields_and_duration_excludes_hooks() -> TestResult {
    let tool = MockTool::new("echo", json!({"type":"object"}), |_, _, _, _| {
        Box::pin(async {
            sleep(Duration::from_millis(34)).await;
            Ok(AgentToolResult {
                details: Some(json!({"keep":true})),
                structured_content: Some(json!({"old":true})),
                usage: Some(pi_rs_ai::Usage {
                    output: 1.0,
                    ..Default::default()
                }),
                is_error: Some(true),
                ..AgentToolResult::text("original")
            })
        })
    });
    let context = ctx(vec![Arc::new(tool)]);
    let assistant = answer("try");
    let signal = CancellationToken::new();
    for (replace_content, patch, expected) in [
        (true, None, None),
        (false, Some(json!({"new":true})), Some(json!({"new":true}))),
        (true, Some(json!({"new":true})), Some(json!({"new":true}))),
        (false, None, Some(json!({"old":true}))),
        (false, Some(Value::Null), Some(json!({"old":true}))),
        (true, Some(Value::Null), None),
    ] {
        let hooks = ToolCallHooks {
            before_tool_call: Some(Arc::new(|_, _| {
                Box::pin(async {
                    sleep(Duration::from_millis(200)).await;
                    Ok(None)
                })
            })),
            after_tool_call: Some(Arc::new(move |context, _| {
                let structured_content = patch.clone();
                Box::pin(async move {
                    sleep(Duration::from_millis(200)).await;
                    assert!(context.is_error);
                    assert_eq!(context.result.usage.map(|u| u.output), Some(1.0));
                    Ok(Some(AfterToolCallResult {
                        content: replace_content.then(|| AgentToolResult::text("redacted").content),
                        structured_content,
                        details: Some(Value::Null),
                        usage: Some(pi_rs_ai::Usage {
                            output: 9.0,
                            ..Default::default()
                        }),
                        is_error: Some(false),
                        terminate: Some(true),
                    }))
                })
            })),
        };
        let outcome = run_tool_call(
            &echo_call("one", "a"),
            RunToolCallOptions {
                tools: &context.tools,
                assistant_message: &assistant,
                context: &context,
                hooks: &hooks,
                signal: &signal,
                on_update: None,
            },
        )
        .await;
        assert_eq!(outcome.duration_ms, Some(34));
        assert!(!outcome.is_error);
        assert_eq!(
            input_text(&outcome.result.content),
            if replace_content {
                "redacted"
            } else {
                "original"
            }
        );
        assert_eq!(outcome.result.details, Some(json!({"keep":true})));
        assert_eq!(outcome.result.structured_content, expected);
        assert_eq!(outcome.result.usage.map(|usage| usage.output), Some(9.0));
        assert_eq!(outcome.result.terminate, Some(true));
    }
    Ok(())
}

#[tokio::test]
async fn only_unanimous_termination_stops_requests_and_blocked_calls_skip_the_after_hook()
-> TestResult {
    for (block_both, patch_terminate, expected_requests) in
        [(true, false, 1), (false, false, 2), (false, true, 1)]
    {
        let mut config = AgentLoopConfig::new(model());
        config.hooks.tool_calls.before_tool_call = Some(Arc::new(move |context, _| {
            let result =
                (block_both || context.tool_call.id == "1").then(|| BeforeToolCallResult {
                    block: true,
                    reason: Some("denied".into()),
                    terminate: true,
                });
            Box::pin(std::future::ready(Ok(result)))
        }));
        let after_calls = Arc::new(Mutex::new(Vec::new()));
        let seen = after_calls.clone();
        config.hooks.tool_calls.after_tool_call = Some(Arc::new(move |context, _| {
            locked(&seen).push(context.tool_call.id.clone());
            Box::pin(std::future::ready(Ok(Some(AfterToolCallResult {
                terminate: Some(patch_terminate),
                ..Default::default()
            }))))
        }));
        let (provider, requests) = scripted([
            tool_reply(vec![echo_call("1", "a"), echo_call("2", "b")]),
            answer("done"),
        ]);
        let (_, messages) = collect(agent_loop(
            go(),
            ctx(vec![Arc::new(MockTool::echo("echo", false))]),
            config,
            None,
            Some(provider),
        )?)
        .await?;
        assert_eq!(requests.users().len(), expected_requests);
        let second = if block_both {
            assert!(locked(&after_calls).is_empty());
            ("2", "denied".into(), true)
        } else {
            assert_eq!(*locked(&after_calls), ["2"]);
            ("2", "echo:b".into(), false)
        };
        assert_eq!(
            tool_texts(&messages),
            [("1", "denied".into(), true), second]
        );
    }
    Ok(())
}

#[tokio::test]
async fn tool_and_hook_errors_are_model_visible_without_losing_reported_details() -> TestResult {
    let tool = MockTool::new("echo", json!({"type":"object"}), |id, _, _, _| {
        Box::pin(async move {
            if id == "failed" {
                return Err(AgentError::Callback("execute error".into()));
            }
            Ok(AgentToolResult {
                details: Some(json!({"partial":true})),
                is_error: Some(true),
                ..AgentToolResult::text("reported error")
            })
        })
    });
    let mut config = AgentLoopConfig::new(model());
    config.hooks.tool_calls.after_tool_call = Some(Arc::new(|context, _| {
        Box::pin(async move {
            assert!(context.is_error);
            if context.tool_call.id == "after" {
                return Err(AgentError::Callback("after error".into()));
            }
            Ok(None)
        })
    }));
    let (provider, _) = scripted([
        tool_reply(vec![
            echo_call("failed", "a"),
            echo_call("after", "b"),
            echo_call("reported", "c"),
        ]),
        answer("ok"),
    ]);
    let (_, messages) = collect(agent_loop(
        go(),
        ctx(vec![Arc::new(tool)]),
        config,
        None,
        Some(provider),
    )?)
    .await?;
    assert_eq!(
        tool_texts(&messages),
        [
            ("failed", "execute error".into(), true),
            ("after", "after error".into(), true),
            ("reported", "reported error".into(), true)
        ]
    );
    let reported = messages.iter().find_map(|m| match m {
        AgentMessage::ToolResult(result) if result.tool_call_id == "reported" => {
            result.details.as_ref()
        }
        _ => None,
    });
    assert_eq!(reported, Some(&json!({"partial":true})));
    Ok(())
}

#[tokio::test]
async fn tool_end_waits_for_every_update_but_updates_can_make_progress_independently() -> TestResult
{
    let release = Arc::new(Notify::new());
    let tool = MockTool::new("echo", json!({"type":"object"}), |_, _, _, update| {
        Box::pin(async move {
            update(AgentToolResult::text("first"));
            update(AgentToolResult::text("second"));
            Ok(AgentToolResult {
                terminate: Some(true),
                ..AgentToolResult::text("finished")
            })
        })
    });
    let trace = Mutex::new(Vec::new());
    let emit = |event| {
        let trace = &trace;
        let release = &release;
        Box::pin(async move {
            match event {
                AgentEvent::ToolExecutionUpdate { partial_result, .. } => {
                    let text = input_text(&partial_result.content);
                    locked(trace).push(text.clone());
                    if text == "first" {
                        release.notified().await;
                        locked(trace).push("first settled".into());
                    } else {
                        release.notify_one();
                    }
                }
                AgentEvent::ToolExecutionEnd { .. } => locked(trace).push("end".into()),
                _ => {}
            }
            Ok(())
        }) as _
    };
    let (provider, _) = scripted([tool_reply(vec![echo_call("one", "a")])]);
    timeout(
        Duration::from_secs(2),
        run_agent_loop(
            go(),
            ctx(vec![Arc::new(tool)]),
            AgentLoopConfig::new(model()),
            &emit,
            None,
            Some(provider),
        ),
    )
    .await??;
    assert_eq!(*locked(&trace), ["first", "second", "first settled", "end"]);
    Ok(())
}
