use super::*;
use pi_rs_ai::{
    AssistantMessageEvent, Context, ImageContent, ImageContentKind, Models, ModelsOptions,
    SystemMessage, Transport, UserAgent,
    faux::{FauxStep, faux_assistant_message, faux_thinking},
    transcript::get_current_tools,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::{sync::Notify, time::timeout};

fn agent(stream_fn: StreamFn, initial_state: AgentInitialState) -> TestResult<Agent> {
    Ok(Agent::new(AgentOptions {
        initial_state,
        stream_fn: Some(stream_fn),
        ..AgentOptions::default()
    })?)
}

#[tokio::test]
async fn installed_default_services_legacy_loop_and_agent_callers() -> TestResult {
    struct ClearDefault;
    impl Drop for ClearDefault {
        fn drop(&mut self) {
            set_default_stream_fn(None);
        }
    }
    let _clear = ClearDefault;
    let (stream, requests) = scripted([answer("from agent"), answer("from loop")]);
    set_default_stream_fn(Some(stream));
    let agent = Agent::new(AgentOptions::default())?;
    set_default_stream_fn(None);
    assert!(matches!(
        Agent::new(AgentOptions::default()),
        Err(AgentError::MissingStreamFunction)
    ));
    agent.prompt("agent request").await?; // The controller retains the fallback chosen at construction.
    let (replacement, loop_requests) = scripted([answer("replacement")]);
    set_default_stream_fn(Some(replacement));
    let stream = agent_loop(
        vec![AgentMessage::user("loop request")],
        AgentContext::default(),
        AgentLoopConfig::new(model()),
        None,
        None,
    )?;
    assert!(
        matches!(stream.result().await?.last(), Some(AgentMessage::Assistant(message)) if message.content == [faux_text("replacement")])
    );
    assert_eq!(requests.users(), [["agent request"]]);
    assert_eq!(loop_requests.users(), [["loop request"]]);
    Ok(())
}

#[tokio::test]
async fn tool_loadout_deltas_replace_pending_intent_and_reset_replays_the_prompt() -> TestResult {
    let (stream, requests) = scripted([answer("one"), answer("two"), answer("three")]);
    let tool = Arc::new(MockTool::echo("first", false));
    let agent = agent(
        stream,
        AgentInitialState {
            system_prompt: Some("base".into()),
            tools: vec![tool],
            ..AgentInitialState::default()
        },
    )?;
    let initial = agent.messages();
    assert!(
        matches!(&initial[0], AgentMessage::System(system) if system.timestamp == 0 && system.tools_added.as_ref().is_some_and(|t| t[0].name == "first"))
    );
    let new_tool = Arc::new(MockTool::echo("second", false));
    agent.set_tools(vec![new_tool]);
    let falsified_tool = MockTool::echo("faked", false).declaration;
    let patch = SystemMessage {
        timestamp: 1,
        content: StringOrBlocks::String("more rules".into()),
        tools_added: Some(vec![falsified_tool]),
        ..Default::default()
    };
    agent
        .prompt(vec![patch.into(), AgentMessage::user("first")])
        .await?;
    agent.prompt("second").await?;
    // Change a declaration in place: same name replaces both schema and ordered tool slot.
    let mut revised = MockTool::echo("second", false);
    revised.declaration.description = "revision".into();
    agent.set_tools(vec![Arc::new(revised)]);
    agent.prompt("third").await?;
    {
        let requests = locked(&requests.0);
        for (_, ctx, _) in &*requests {
            let tools = get_current_tools(ctx.messages());
            assert_eq!(
                tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>(),
                ["second"]
            );
        }
        assert!(
            matches!(&requests[0].1.messages()[1], Message::System(s) if s.tools_removed.as_ref().is_some_and(|v| v[0].name == "first") && s.timestamp == 1)
        );
        assert_eq!(
            requests[1]
                .1
                .messages()
                .iter()
                .filter(|m| matches!(m, Message::System(_)))
                .count(),
            2
        );
        assert_eq!(
            get_current_tools(requests[2].1.messages())[0].description,
            "revision"
        );
    }
    agent.steer(AgentMessage::user("discard steering"));
    agent.follow_up(AgentMessage::user("discard follow-up"));
    agent.reset()?;
    assert!(!agent.has_queued_messages());
    assert_eq!(agent.system_prompt(), "base\n\nmore rules");
    let messages = agent.messages();
    assert!(
        matches!(messages.as_slice(), [AgentMessage::System(baseline)] if baseline.tools_added.as_ref().is_some_and(|v| v[0].description == "revision"))
    );
    Ok(())
}

#[tokio::test]
async fn subscriptions_see_reduced_state_and_idle_waits_for_agent_end_in_order() -> TestResult {
    let (stream, _) = scripted([answer("first"), answer("second")]);
    let agent = agent(stream, AgentInitialState::default())?;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let inside_end = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let observer_agent = agent.clone();
    let observer_seen = seen.clone();
    let entered = inside_end.clone();
    let released = release.clone();
    let first = agent.subscribe(Arc::new(move |event, signal| {
        let (agent, seen, entered, release) = (
            observer_agent.clone(),
            observer_seen.clone(),
            entered.clone(),
            released.clone(),
        );
        Box::pin(async move {
            assert!(!signal.is_cancelled());
            locked(&seen).push((1, event_type(event)));
            if matches!(event, AgentEvent::AgentEnd { .. }) {
                assert!(agent.state().is_streaming);
                assert!(agent.state().streaming_message.is_none());
                assert!(matches!(
                    agent.messages().last(),
                    Some(AgentMessage::Assistant(_))
                ));
                entered.notify_one();
                release.notified().await;
            }
            Ok(())
        })
    }));
    let observer_seen = seen.clone();
    let listener: Arc<AgentListener> = Arc::new(move |event, _| {
        locked(&observer_seen).push((2, event_type(event)));
        Box::pin(std::future::ready(Ok(())))
    });
    let _second = agent.subscribe(listener.clone());
    let duplicate = agent.subscribe(listener);
    let running = agent.clone();
    let prompt = tokio::spawn(async move { running.prompt("start").await });
    timeout(Duration::from_secs(2), inside_end.notified()).await?;
    assert!(matches!(
        agent.prompt("overlap").await,
        Err(AgentError::PromptInProgress)
    ));
    assert!(matches!(
        agent.continue_run().await,
        Err(AgentError::ContinueInProgress)
    ));
    assert!(matches!(agent.reset(), Err(AgentError::ResetInProgress)));
    let idle = agent.wait_for_idle();
    tokio::pin!(idle);
    assert!(futures_util::poll!(&mut idle).is_pending());
    assert_eq!(locked(&seen).last(), Some(&(1, "agent_end")));
    release.notify_one();
    timeout(Duration::from_secs(2), idle).await?;
    prompt.await??;
    assert!(!agent.state().is_streaming);
    assert_eq!(locked(&seen).last(), Some(&(2, "agent_end")));
    {
        let seen = locked(&seen);
        let (paired, remainder) = seen.as_chunks::<2>();
        assert!(remainder.is_empty());
        assert!(
            paired
                .iter()
                .all(|[first, second]| (first.0, second.0) == (1, 2) && first.1 == second.1)
        );
    }
    assert!(first.unsubscribe());
    assert!(duplicate.unsubscribe());
    let count = locked(&seen).len();
    agent.prompt("again").await?;
    assert_eq!(locked(&seen).len(), count);
    Ok(())
}

#[tokio::test]
async fn continuations_preserve_queue_modes_and_steering_priority_for_both_tails() -> TestResult {
    for assistant_tail in [false, true] {
        for mode in [QueueMode::OneAtATime, QueueMode::All] {
            let (stream, requests) = scripted((0..4).map(|_| answer("ok")));
            let agent = agent(stream, AgentInitialState::default())?;
            let mut context = vec![AgentMessage::user("existing")];
            if assistant_tail {
                context.push(answer("previous").into());
            }
            agent.set_messages(context);
            agent.set_steering_mode(mode);
            agent.set_follow_up_mode(mode);
            let one = AgentMessage::user("steer one");
            let two = AgentMessage::user("steer two");
            agent.steer(one.clone());
            agent.steer(two.clone());
            agent.follow_up(AgentMessage::user("follow one"));
            agent.follow_up(AgentMessage::user("follow two"));
            let preview = if mode == QueueMode::All {
                vec![one, two]
            } else {
                vec![one]
            };
            assert_eq!(agent.peek_queued_messages(), preview);
            assert_eq!(agent.peek_queued_messages(), preview);
            agent.continue_run().await?;
            let all = vec![
                "existing",
                "steer one",
                "steer two",
                "follow one",
                "follow two",
            ];
            if mode == QueueMode::OneAtATime {
                assert_eq!(
                    requests.users(),
                    [all[..2].to_vec(), all[..3].to_vec(), all[..4].to_vec(), all]
                );
            } else {
                assert_eq!(requests.users(), [all[..3].to_vec(), all]);
            }
            assert!(!agent.has_queued_messages());
            assert_eq!(agent.state().error_message, None);
        }
    }
    Ok(())
}

#[tokio::test]
async fn validates_continuation_without_draining_queues_and_defers_followups_on_retry() -> TestResult
{
    let (stream, requests) = scripted([answer("retry"), answer("followed")]);
    let agent = agent(stream, AgentInitialState::default())?;
    let steer = AgentMessage::user("steering");
    let follow = AgentMessage::user("following");
    agent.steer(steer.clone());
    agent.follow_up(follow.clone());
    for with_system in [false, true] {
        if with_system {
            agent.append_system_prompt("rules");
        }
        assert!(matches!(
            agent.continue_run().await,
            Err(AgentError::NoConversation)
        ));
        assert_eq!(agent.peek_queued_messages(), std::slice::from_ref(&steer));
    }
    agent.clear_steering_queue();
    assert_eq!(agent.peek_queued_messages(), [follow]);
    agent.append_message(AgentMessage::user("retry this"));
    agent.continue_run().await?;
    assert_eq!(
        requests.users(),
        [vec!["retry this"], vec!["retry this", "following"]]
    );
    assert!(matches!(
        agent.continue_run().await,
        Err(AgentError::AssistantTail)
    ));
    Ok(())
}

#[tokio::test]
async fn restores_state_after_stream_contract_failure_and_preserves_queued_messages() -> TestResult
{
    let stream: StreamFn = Arc::new(|_, _, _| {
        Box::pin(async { Err(AgentError::Callback("provider exploded".into())) })
    });
    let agent = agent(stream, AgentInitialState::default())?;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let trace = seen.clone();
    let listener_agent = agent.clone();
    let unsubscribe = agent.subscribe(Arc::new(move |event, _| {
        locked(&trace).push(event_type(event));
        if matches!(
            event,
            AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(_)
            }
        ) {
            listener_agent.steer(AgentMessage::user("later"));
        }
        Box::pin(std::future::ready(Ok(())))
    }));
    agent.follow_up(AgentMessage::user("follow"));
    agent.prompt("hello").await?;
    unsubscribe.unsubscribe();
    assert_eq!(
        *locked(&seen),
        [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end"
        ]
    );
    assert!(
        matches!(agent.messages().last(), Some(AgentMessage::Assistant(a)) if a.stop_reason == StopReason::Error)
    );
    assert_eq!(
        agent.state().error_message.as_deref(),
        Some("provider exploded")
    );
    assert!(!agent.state().is_streaming);
    assert!(
        matches!(agent.peek_queued_messages().as_slice(), [AgentMessage::User(u)] if u.content == StringOrBlocks::String("later".into()))
    );
    agent.clear_steering_queue();
    assert!(
        matches!(agent.peek_queued_messages().as_slice(), [AgentMessage::User(u)] if u.content == StringOrBlocks::String("follow".into()))
    );
    Ok(())
}

#[tokio::test]
async fn settled_tool_cannot_emit_progress_while_parallel_peer_is_running() -> TestResult {
    let saved = Arc::new(Mutex::new(None::<AgentToolUpdateCallback>));
    let update = saved.clone();
    let fast = MockTool::new("fast", json!({"type":"object"}), move |_, _, _, cb| {
        cb(AgentToolResult::text("working"));
        *locked(&update) = Some(cb);
        Box::pin(std::future::ready(Ok(AgentToolResult {
            terminate: Some(true),
            ..AgentToolResult::text("fast")
        })))
    });
    let release = Arc::new(Notify::new());
    let tool_release = release.clone();
    let slow = MockTool::new("slow", json!({"type":"object"}), move |_, _, _, _| {
        let release = tool_release.clone();
        Box::pin(async move {
            release.notified().await;
            Ok(AgentToolResult {
                terminate: Some(true),
                ..AgentToolResult::text("slow")
            })
        })
    });
    let (stream, _) = scripted([tool_reply(vec![
        call("fast", "fast", []),
        call("slow", "slow", []),
    ])]);
    let agent = agent(
        stream,
        AgentInitialState {
            tools: vec![Arc::new(fast), Arc::new(slow)],
            ..AgentInitialState::default()
        },
    )?;
    let fast_ended = Arc::new(Notify::new());
    let notify = fast_ended.clone();
    let updates = Arc::new(Mutex::new(Vec::new()));
    let observed = updates.clone();
    let observer = agent.clone();
    let sub = agent.subscribe(Arc::new(move |event, _| {
        match event {
            AgentEvent::ToolExecutionUpdate { partial_result, .. } => {
                locked(&observed).push(input_text(&partial_result.content))
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } if tool_call_id == "fast" => {
                assert_eq!(
                    observer
                        .state()
                        .pending_tool_calls
                        .into_iter()
                        .collect::<Vec<_>>(),
                    ["slow"]
                );
                notify.notify_one();
            }
            _ => {}
        }
        Box::pin(std::future::ready(Ok(())))
    }));
    let running = agent.clone();
    let prompt = tokio::spawn(async move { running.prompt("run").await });
    timeout(Duration::from_secs(2), fast_ended.notified()).await?;
    let cb = locked(&saved)
        .take()
        .ok_or("did not capture tool progress callback")?;
    cb(AgentToolResult::text("late before idle"));
    release.notify_one();
    prompt.await??;
    cb(AgentToolResult::text("late after idle"));
    sub.unsubscribe();
    assert_eq!(locked(&updates).as_slice(), ["working"]);
    assert!(agent.state().pending_tool_calls.is_empty());
    Ok(())
}

#[tokio::test]
async fn runtime_options_and_observer_are_forwarded_with_identity_inheritance_and_override()
-> TestResult {
    let (base, requests) = scripted([answer("one"), answer("two"), answer("three")]);
    let provider_events = Arc::new(Mutex::new(Vec::new()));
    let log = provider_events.clone();
    let stream_fn: StreamFn = Arc::new(move |model, context, options| {
        let base = base.clone();
        Box::pin(async move {
            if let Some(observer) = &options.on_provider_stream_event {
                observer(&json!({"request_cost":0.01}), &model).await?;
            }
            base(model, context, options).await
        })
    });
    let stream_options = StreamOptions {
        on_provider_stream_event: Some(Arc::new(move |event, _| {
            locked(&log).push(event.clone());
            Box::pin(std::future::ready(Ok(())))
        })),
        ..Default::default()
    };
    let agent = Agent::new(AgentOptions {
        stream_options,
        stream_fn: Some(stream_fn),
        ..Default::default()
    })?;
    agent.set_session_id(Some("s1".into()));
    agent
        .prompt_with_images(
            "first",
            vec![ImageContent {
                kind: ImageContentKind::Image,
                data: "b2s=".into(),
                mime_type: "image/png".into(),
            }],
        )
        .await?;
    agent.set_session_id(Some("s2".into()));
    agent.set_transport(Transport::Sse);
    agent.set_thinking_level(ThinkingLevel::High);
    agent.set_thinking_budgets([(ThinkingLevel::High, 200)].into_iter().collect());
    agent.set_max_retry_delay_ms(Some(100));
    let mut opts = agent.stream_options();
    opts.request.identity.user_agent = Some(UserAgent::Short);
    agent.set_stream_options(opts);
    agent.prompt("second").await?;
    let mut opts = agent.stream_options();
    opts.request.identity.attribution = Some(None);
    agent.set_stream_options(opts);
    agent.prompt("third").await?;
    assert_eq!(
        *locked(&provider_events),
        vec![json!({"request_cost":0.01}); 3]
    );
    let reqs = locked(&requests.0);
    assert_eq!(reqs[0].2.session_id.as_deref(), Some("s1"));
    assert_eq!(
        reqs[0].2.request.identity.attribution,
        DEFAULT_AGENT_IDENTITY.attribution
    );
    assert!(
        matches!(&reqs[0].1.messages()[0], Message::User(u) if matches!(&u.content, StringOrBlocks::Blocks(blocks) if matches!(&blocks[1], InputContent::Image(image) if image.data == "b2s=")))
    );
    assert_eq!(reqs[1].2.session_id.as_deref(), Some("s2"));
    assert_eq!(reqs[1].2.transport, Some(Transport::Sse));
    assert_eq!(reqs[1].2.thinking_budgets[&ThinkingLevel::High], 200);
    assert_eq!(reqs[1].2.request.max_retry_delay_ms, Some(100));
    assert_eq!(
        reqs[1].2.request.identity.user_agent,
        Some(UserAgent::Short)
    );
    assert_eq!(
        reqs[1].2.request.identity.attribution,
        DEFAULT_AGENT_IDENTITY.attribution
    );
    assert_eq!(reqs[2].2.request.identity.attribution, Some(None));
    Ok(())
}

#[tokio::test]
async fn faux_registry_streams_thinking_executes_tools_and_continues_from_committed_results()
-> TestResult {
    let faux = faux_provider(FauxOptions {
        token_size: Some((2, 2)),
        ..Default::default()
    });
    let selected = faux.get_model(None).ok_or("faux model missing")?.clone();
    let models = Models::new(ModelsOptions::default());
    models.set_provider(faux.provider.clone());
    faux.set_responses([
        tool_reply(vec![call("calc", "echo", [("value", json!("Alice"))])]).into(),
        FauxStep::Factory(Arc::new(|ctx, _, _, _| Box::pin(async move {
            assert!(ctx.messages().iter().any(|msg| matches!(msg, Message::ToolResult(tool) if tool.tool_call_id == "calc" && input_text(&tool.content) == "echo:Alice")));
            Ok(faux_assistant_message(vec![faux_thinking("considering context"), faux_text("Hello Alice")]))
        }))),
        FauxStep::Factory(Arc::new(|ctx, _, _, _| Box::pin(async move {
            assert!(ctx.messages().iter().any(|msg| matches!(msg, Message::Assistant(msg) if msg.content.contains(&faux_text("Hello Alice")))));
            Ok(faux_assistant_message(vec![faux_text("I remember")]))
        }))),
    ]);
    let agent = agent(
        models_stream_fn(models),
        AgentInitialState {
            model: Some(selected),
            system_prompt: Some("respond locally".into()),
            tools: vec![Arc::new(MockTool::echo("echo", false))],
            ..Default::default()
        },
    )?;
    let updates = Arc::new(AtomicUsize::new(0));
    let seen = updates.clone();
    let _subscription = agent.subscribe(Arc::new(move |event, _| {
        if matches!(event, AgentEvent::MessageUpdate { .. }) {
            seen.fetch_add(1, Ordering::Relaxed);
        }
        Box::pin(std::future::ready(Ok(())))
    }));
    agent.prompt("I am Alice").await?;
    assert_eq!(
        tool_texts(&agent.messages()),
        [("calc", "echo:Alice".into(), false)]
    );
    assert!(
        matches!(agent.messages().last(), Some(AgentMessage::Assistant(message)) if message.content == [faux_thinking("considering context"), faux_text("Hello Alice")])
    );
    agent.append_message(AgentMessage::user("who am I?"));
    agent.continue_run().await?;
    assert!(
        matches!(agent.messages().last(), Some(AgentMessage::Assistant(msg)) if msg.content == [faux_text("I remember")])
    );
    assert!(updates.load(Ordering::Relaxed) > 5);
    assert_eq!(faux.state.call_count(), 3);
    Ok(())
}

#[tokio::test]
async fn aborting_a_running_faux_response_sets_the_shared_signal_and_transcript_error() -> TestResult
{
    let faux = faux_provider(FauxOptions {
        token_size: Some((1, 1)),
        tokens_per_second: Some(20.0),
        ..Default::default()
    });
    let selected = faux.get_model(None).ok_or("faux model missing")?.clone();
    let models = Models::new(ModelsOptions::default());
    models.set_provider(faux.provider.clone());
    faux.set_responses([faux_assistant_message(vec![faux_text(
        "a deliberately slow streamed response",
    )])
    .into()]);
    let agent = agent(
        models_stream_fn(models),
        AgentInitialState {
            model: Some(selected),
            ..Default::default()
        },
    )?;
    let started = Arc::new(Notify::new());
    let notice = started.clone();
    let active = Arc::new(Mutex::new(None::<CancellationToken>));
    let listener_signal = active.clone();
    let _sub = agent.subscribe(Arc::new(move |event, signal| {
        if let AgentEvent::MessageUpdate {
            assistant_message_event: AssistantMessageEvent::TextDelta { .. },
            ..
        } = event
        {
            *locked(&listener_signal) = Some(signal.clone());
            notice.notify_one();
        }
        Box::pin(std::future::ready(Ok(())))
    }));
    let running = agent.clone();
    let prompt = tokio::spawn(async move { running.prompt("go").await });
    timeout(Duration::from_secs(2), started.notified()).await?;
    agent.abort();
    timeout(Duration::from_secs(2), agent.wait_for_idle()).await?;
    prompt.await??;
    let state = agent.state();
    assert!(
        locked(&active)
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    );
    assert!(
        matches!(state.messages.last(), Some(AgentMessage::Assistant(msg)) if msg.stop_reason == StopReason::Aborted && msg.error_message.is_some() && msg.error_message == state.error_message)
    );
    assert!(!state.is_streaming);
    Ok(())
}

#[tokio::test]
async fn continuation_after_tool_result_sends_no_duplicate_messages() -> TestResult {
    let (stream, requests) = scripted([answer("read persisted result")]);
    let context = pi_rs_ai::normalize_context(Context {
        messages: vec![Message::ToolResult(pi_rs_ai::ToolResultMessage {
            role: pi_rs_ai::ToolResultRole::ToolResult,
            tool_call_id: "one".into(),
            tool_name: "echo".into(),
            content: AgentToolResult::text("persisted").content,
            details: None,
            usage: None,
            nested_calls: None,
            is_error: false,
            timestamp: 1,
            duration_ms: None,
        })],
        ..Default::default()
    });
    let agent = agent(
        stream,
        AgentInitialState {
            messages: context
                .into_messages()
                .into_iter()
                .map(AgentMessage::from)
                .collect(),
            ..Default::default()
        },
    )?;
    agent.continue_run().await?;
    assert!(
        matches!(locked(&requests.0)[0].1.messages(), [Message::ToolResult(tool)] if input_text(&tool.content) == "persisted")
    );
    assert_eq!(
        agent
            .messages()
            .iter()
            .map(AgentMessage::role)
            .collect::<Vec<_>>(),
        ["toolResult", "assistant"]
    );
    Ok(())
}
