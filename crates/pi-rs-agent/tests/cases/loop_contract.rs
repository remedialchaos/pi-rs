use super::*;
use pi_rs_ai::SystemMessage;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[tokio::test]
async fn transforms_before_conversion_and_emits_the_full_prompt_lifecycle() -> TestResult {
    let custom = AgentMessage::custom(
        "notification",
        JsonObject::from_iter([
            ("text".into(), json!("local")),
            ("timestamp".into(), json!(4)),
        ]),
    )?;
    assert_eq!(
        serde_json::to_value(&custom)?,
        json!({"role":"notification","text":"local","timestamp":4})
    );
    assert_eq!(
        serde_json::from_value::<AgentMessage>(serde_json::to_value(&custom)?)?,
        custom
    );
    let mut config = AgentLoopConfig::new(model());
    config.hooks.transform_context = Some(Arc::new(|messages, _| {
        Box::pin(async move {
            assert_eq!(
                messages.iter().map(AgentMessage::role).collect::<Vec<_>>(),
                ["user", "system", "notification", "user"]
            );
            messages[1..].to_vec()
        })
    }));
    config.hooks.convert_to_llm = Some(Arc::new(|messages| {
        Box::pin(async move {
            messages
                .iter()
                .filter_map(|message| {
                    if let AgentMessage::Custom(custom) = message {
                        return AgentMessage::user(custom.fields["text"].as_str()?).to_llm();
                    }
                    message.to_llm()
                })
                .collect()
        })
    }));
    let context = AgentContext {
        messages: vec![AgentMessage::user("pruned")],
        ..AgentContext::default()
    };
    let system = AgentMessage::from(SystemMessage {
        content: StringOrBlocks::String("rules".into()),
        ..Default::default()
    });
    let (provider, requests) = scripted([answer("reply")]);
    let (events, messages) = collect(agent_loop(
        vec![system, custom, AgentMessage::user("hello")],
        context,
        config,
        None,
        Some(provider),
    )?)
    .await?;
    assert_eq!(requests.users(), [["local", "hello"]]);
    let payload = serde_json::to_value(&locked(&requests.0)[0].1)?;
    assert_eq!(
        payload["messages"][0],
        json!({"role":"system","content":"rules","timestamp":0})
    );
    assert_eq!(payload.as_object().map(|o| o.len()), Some(1));
    assert_eq!(
        events.iter().map(event_type).collect::<Vec<_>>(),
        [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end"
        ]
    );
    assert_eq!(
        messages.iter().map(AgentMessage::role).collect::<Vec<_>>(),
        ["system", "notification", "user", "assistant"]
    );
    let encoded = serde_json::to_value(&events[10])?;
    assert_eq!(encoded["type"], "turn_end");
    assert_eq!(encoded["message"]["thinkingLevel"], "off");
    assert_eq!(encoded["toolResults"], json!([]));
    Ok(())
}

#[tokio::test]
async fn continuation_rejects_empty_and_assistant_tails_but_allows_custom_conversion() -> TestResult
{
    let (provider, requests) = scripted([answer("resume")]);
    let config = AgentLoopConfig::new(model());
    assert!(matches!(
        agent_loop_continue(
            AgentContext::default(),
            config.clone(),
            None,
            Some(provider.clone())
        ),
        Err(AgentError::EmptyContext)
    ));
    let context = AgentContext {
        messages: vec![answer("no").into()],
        ..AgentContext::default()
    };
    assert!(matches!(
        agent_loop_continue(context, config.clone(), None, Some(provider.clone())),
        Err(AgentError::AssistantTail)
    ));
    let mut config = config;
    config.hooks.convert_to_llm = Some(Arc::new(|messages| {
        Box::pin(async move {
            messages
                .iter()
                .map(|message| match message {
                    AgentMessage::Custom(_) => AgentMessage::user("from custom"),
                    other => other.clone(),
                })
                .filter_map(|m| m.to_llm())
                .collect()
        })
    }));
    let context = AgentContext {
        messages: vec![AgentMessage::custom("resume", JsonObject::new())?],
        ..AgentContext::default()
    };
    let (events, messages) =
        collect(agent_loop_continue(context, config, None, Some(provider))?).await?;
    assert_eq!(requests.users(), [["from custom"]]);
    assert_eq!(
        messages.iter().map(AgentMessage::role).collect::<Vec<_>>(),
        ["assistant"]
    );
    assert_eq!(
        events.iter().map(event_type).collect::<Vec<_>>(),
        [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end"
        ]
    );
    Ok(())
}

#[tokio::test]
async fn finish_turn_sees_committed_tool_results_and_end_preserves_input_queues() -> TestResult {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let hook_trace = trace.clone();
    let steering = Arc::new(AtomicUsize::new(0));
    let steering_count = steering.clone();
    let mut config = AgentLoopConfig::new(model());
    config.get_steering_messages = Some(Arc::new(move || {
        steering_count.fetch_add(1, Ordering::Relaxed);
        Box::pin(std::future::ready(Vec::new()))
    }));
    config.get_follow_up_messages = Some(Arc::new(|| {
        Box::pin(async { unreachable!("finish_turn ended the run before queue polling") })
    }));
    config.hooks.prepare_next_turn = Some(Arc::new(|_, _| {
        Box::pin(async { unreachable!("finish_turn ended the run before next-turn preparation") })
    }));
    config.hooks.finish_turn = Some(Arc::new(move |context, _| {
        assert!(
            matches!(context.context.messages.last(), Some(AgentMessage::ToolResult(result)) if result.tool_call_id == "a")
        );
        assert_eq!(context.tool_results[0].tool_call_id, "a");
        assert_eq!(
            context
                .new_messages
                .iter()
                .map(AgentMessage::role)
                .collect::<Vec<_>>(),
            ["system", "user", "assistant", "toolResult"]
        );
        locked(&hook_trace).push("finish_turn");
        Box::pin(std::future::ready(Ok(Some(AgentTurnDecision::End))))
    }));
    let emit = |event: AgentEvent| {
        let marker = match &event {
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult(_),
            } => "result",
            _ => event_type(&event),
        };
        locked(&trace).push(marker);
        Box::pin(std::future::ready(Ok(()))) as _
    };
    let (provider, requests) = scripted([tool_reply(vec![call(
        "a",
        "echo",
        [("value", json!("yes"))],
    )])]);
    run_agent_loop(
        vec![AgentMessage::user("start")],
        AgentContext {
            messages: Vec::new(),
            tools: vec![Arc::new(MockTool::echo("echo", false))],
        },
        config,
        &emit,
        None,
        Some(provider),
    )
    .await?;
    assert_eq!(requests.users().len(), 1);
    assert_eq!(steering.load(Ordering::Relaxed), 1);
    let trace = locked(&trace);
    assert_eq!(
        trace.as_slice(),
        [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "tool_execution_start",
            "tool_execution_end",
            "message_start",
            "result",
            "finish_turn",
            "turn_end",
            "agent_end"
        ]
    );
    Ok(())
}

#[tokio::test]
async fn finish_continue_uses_a_natural_request_or_adds_one_context_only_turn() -> TestResult {
    for natural in [None, Some("steering"), Some("follow-up"), Some("tool")] {
        let finish = AtomicBool::new(true);
        let polls = AtomicUsize::new(0);
        let followed = AtomicBool::new(false);
        let mut config = AgentLoopConfig::new(model());
        config.hooks.finish_turn = Some(Arc::new(move |_, _| {
            Box::pin(std::future::ready(Ok(finish
                .swap(false, Ordering::Relaxed)
                .then_some(AgentTurnDecision::Continue))))
        }));
        config.get_steering_messages = Some(Arc::new(move || {
            let has = polls.fetch_add(1, Ordering::Relaxed) == 1 && natural == Some("steering");
            Box::pin(std::future::ready(if has {
                vec![AgentMessage::user("steering")]
            } else {
                Vec::new()
            }))
        }));
        config.get_follow_up_messages = Some(Arc::new(move || {
            let has = !followed.swap(true, Ordering::Relaxed) && natural == Some("follow-up");
            Box::pin(std::future::ready(if has {
                vec![AgentMessage::user("follow-up")]
            } else {
                Vec::new()
            }))
        }));
        let has_tool = natural == Some("tool");
        let context = AgentContext {
            messages: Vec::new(),
            tools: if has_tool {
                vec![Arc::new(MockTool::echo("echo", false))]
            } else {
                Vec::new()
            },
        };
        let first = if has_tool {
            tool_reply(vec![call("a", "echo", [("value", json!("ok"))])])
        } else {
            answer("one")
        };
        let (provider, requests) = scripted([first, answer("two")]);
        let (_, messages) = collect(agent_loop(
            vec![AgentMessage::user("start")],
            context,
            config,
            None,
            Some(provider),
        )?)
        .await?;
        let users = requests.users();
        assert_eq!(users.len(), 2, "{natural:?}");
        assert_eq!(users[0], ["start"]);
        assert_eq!(
            users[1],
            if matches!(natural, Some("steering" | "follow-up")) {
                vec!["start", natural.unwrap_or_default()]
            } else {
                vec!["start"]
            }
        );
        if has_tool {
            assert_eq!(tool_texts(&messages), [("a", "echo:ok".into(), false)]);
        }
    }
    Ok(())
}

#[tokio::test]
async fn provider_error_and_abort_are_hard_exits_even_if_finish_requests_continue() -> TestResult {
    for reason in [StopReason::Error, StopReason::Aborted] {
        let observed = Arc::new(AtomicBool::new(false));
        let observed_hook = observed.clone();
        let mut config = AgentLoopConfig::new(model());
        config.hooks.finish_turn = Some(Arc::new(move |turn, _| {
            assert_eq!(turn.message.stop_reason, reason);
            assert!(turn.tool_results.is_empty());
            observed_hook.store(true, Ordering::Relaxed);
            Box::pin(std::future::ready(Ok(Some(AgentTurnDecision::Continue))))
        }));
        config.get_follow_up_messages = Some(Arc::new(|| {
            Box::pin(async { unreachable!("failed turn polled follow-up") })
        }));
        let (provider, requests) = scripted([reply(Vec::new(), reason)]);
        let (events, messages) = collect(agent_loop(
            vec![AgentMessage::user("start")],
            AgentContext::default(),
            config,
            None,
            Some(provider),
        )?)
        .await?;
        assert!(observed.load(Ordering::Relaxed));
        assert_eq!(requests.users().len(), 1);
        assert_eq!(
            events
                .iter()
                .rev()
                .take(2)
                .map(event_type)
                .collect::<Vec<_>>(),
            ["agent_end", "turn_end"]
        );
        assert!(
            matches!(messages.last(), Some(AgentMessage::Assistant(message)) if message.stop_reason == reason)
        );
    }
    Ok(())
}

#[tokio::test]
async fn prepare_request_applies_after_input_and_resolves_keys_each_turn() -> TestResult {
    let queue = Arc::new(Mutex::new(vec![AgentMessage::user("startup steering")]));
    let drain = queue.clone();
    let mut config = AgentLoopConfig::new(model());
    config.stream_options.request.api_key = Some("fallback".into());
    config.get_steering_messages = Some(Arc::new(move || {
        Box::pin(std::future::ready(std::mem::take(&mut *locked(&drain))))
    }));
    let first = AtomicBool::new(true);
    config.hooks.prepare_request = Some(Arc::new(move |context, _| {
        let update = if first.swap(false, Ordering::Relaxed) {
            assert!(
                matches!(context.context.messages.last(), Some(AgentMessage::User(u)) if u.content == StringOrBlocks::String("startup steering".into()))
            );
            locked(&queue).push(AgentMessage::user("steer after prepare"));
            let mut exact = model();
            exact.base.provider = "replacement".into();
            Some(AgentRequestUpdate {
                model: Some(exact),
                thinking_level: Some(ThinkingLevel::High),
                context: Some(AgentContext {
                    messages: vec![AgentMessage::user("canonical")],
                    ..context.context.clone()
                }),
            })
        } else {
            None
        };
        Box::pin(std::future::ready(Ok(update)))
    }));
    let key_calls = AtomicUsize::new(0);
    config.hooks.get_api_key = Some(Arc::new(move |provider| {
        assert_eq!(provider, "replacement");
        let key = if key_calls.fetch_add(1, Ordering::Relaxed) == 0 {
            "fresh"
        } else {
            ""
        };
        Box::pin(std::future::ready(Some(key.into())))
    }));
    let (stream, requests) = scripted([answer("first"), answer("second")]);
    let (_, messages) = collect(agent_loop(
        vec![AgentMessage::user("start")],
        AgentContext::default(),
        config,
        None,
        Some(stream),
    )?)
    .await?;
    assert_eq!(
        requests.users(),
        [vec!["canonical"], vec!["canonical", "steer after prepare"]]
    );
    let reqs = locked(&requests.0);
    assert_eq!(
        reqs.iter()
            .map(|(model, _, options)| (
                model.provider.as_str(),
                options.reasoning,
                options.request.api_key.as_deref()
            ))
            .collect::<Vec<_>>(),
        [
            ("replacement", Some(ThinkingLevel::High), Some("fresh")),
            ("replacement", Some(ThinkingLevel::High), Some("fallback"))
        ]
    );
    assert_eq!(
        messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::Assistant(a) => a.thinking_level,
                _ => None,
            })
            .collect::<Vec<_>>(),
        [ThinkingLevel::High, ThinkingLevel::High]
    );
    Ok(())
}

#[tokio::test]
async fn prepare_next_turn_applies_replacement_and_repolls_only_if_nothing_was_selected()
-> TestResult {
    for queued_early in [false, true] {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let for_poll = queue.clone();
        let calls = AtomicUsize::new(0);
        let finish = queue.clone();
        let mut config = AgentLoopConfig::new(model());
        config.stream_options.reasoning = Some(ThinkingLevel::High);
        config.get_steering_messages = Some(Arc::new(move || {
            let selected = locked(&for_poll).pop_front();
            Box::pin(std::future::ready(selected.into_iter().collect()))
        }));
        config.hooks.finish_turn = Some(Arc::new(move |_, _| {
            let first = calls.fetch_add(1, Ordering::Relaxed) == 0;
            if first && queued_early {
                locked(&finish).push_back(AgentMessage::user("early"));
            }
            Box::pin(std::future::ready(Ok(Some(if first {
                AgentTurnDecision::Continue
            } else {
                AgentTurnDecision::End
            }))))
        }));
        config.hooks.prepare_next_turn = Some(Arc::new(move |turn, _| {
            assert_eq!(turn.message.content, [faux_text("one")]);
            locked(&queue).push_back(AgentMessage::user("during preparation"));
            let mut updated = model();
            updated.base.id = "next".into();
            Box::pin(std::future::ready(Ok(Some(AgentLoopTurnUpdate {
                context: Some(AgentContext {
                    messages: vec![AgentMessage::user("replacement")],
                    ..AgentContext::default()
                }),
                messages: vec![AgentMessage::user("prepared")],
                model: Some(updated),
                thinking_level: Some(ThinkingLevel::Off),
            }))))
        }));
        let (provider, requests) = scripted([answer("one"), answer("two")]);
        collect(agent_loop(
            vec![AgentMessage::user("start")],
            AgentContext::default(),
            config,
            None,
            Some(provider),
        )?)
        .await?;
        let selected = if queued_early {
            "early"
        } else {
            "during preparation"
        };
        assert_eq!(
            requests.users(),
            [vec!["start"], vec!["replacement", "prepared", selected]]
        );
        assert_eq!(locked(&requests.0)[1].0.id, "next");
        assert_eq!(locked(&requests.0)[1].2.reasoning, None);
    }
    Ok(())
}
